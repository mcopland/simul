use std::collections::{BTreeMap, HashSet};

use serde::{Deserialize, Serialize};

use crate::claude_sessions::{Live, LiveStatus};
use crate::hook::{Agent, AgentEvent, Parsed, Screen};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum State {
    Working,
    Blocked,
    Done,
    Idle,
    Unknown,
}

impl State {
    pub fn name(self) -> &'static str {
        match self {
            State::Working => "working",
            State::Blocked => "blocked",
            State::Done => "done",
            State::Idle => "idle",
            State::Unknown => "unknown",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PaneRecord {
    pub pane: String,
    pub agent: Agent,
    pub state: State,
    /// Unix ms of the last state change.
    pub since_ms: u64,
    /// Unix ms of the last hook event; 0 if none yet (recovered panes).
    pub last_event_ms: u64,
    pub session_id: Option<String>,
    pub cwd: Option<String>,
    pub branch: Option<String>,
    pub pending_tool: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub effort: Option<String>,
    /// Transcript file, read after each turn for the current model.
    #[serde(default)]
    pub transcript: Option<String>,
    pub session: Option<String>,
    pub window: Option<u32>,
    #[serde(default)]
    pub window_id: Option<String>,
    #[serde(default)]
    pub window_name: Option<String>,
    /// Whether this is the active pane of its window.
    #[serde(default)]
    pub active: bool,
    /// Consecutive scrapes suggesting a busy pane has settled (idle, or no prompt left).
    #[serde(skip)]
    idle_scrapes: u8,
    /// Status time of the agent's own live session file, while it has one.
    #[serde(skip)]
    live_ms: Option<u64>,
}

impl PaneRecord {
    fn new(pane: &str, agent: Agent, state: State, now: u64) -> PaneRecord {
        PaneRecord {
            pane: pane.to_owned(),
            agent,
            state,
            since_ms: now,
            last_event_ms: 0,
            session_id: None,
            cwd: None,
            branch: None,
            pending_tool: None,
            model: None,
            effort: None,
            transcript: None,
            session: None,
            window: None,
            window_id: None,
            window_name: None,
            active: false,
            idle_scrapes: 0,
            live_ms: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Transition {
    pub pane: String,
    pub agent: Agent,
    pub from: Option<State>,
    pub to: State,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PaneInfo {
    pub id: String,
    pub session: String,
    pub window: u32,
    pub window_id: String,
    pub current_command: String,
    pub path: String,
    pub is_sidebar: bool,
    pub window_panes: u32,
    /// Whether this is the active pane of its window.
    pub active: bool,
    pub window_name: String,
}

/// A point-in-time view of the tmux server used to reconcile tracked panes.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Snapshot {
    pub panes: Vec<PaneInfo>,
    /// Panes the user is looking at right now.
    pub viewed: HashSet<String>,
}

/// How much later than Claude's own clock a hook can reach the daemon (shim start plus socket).
const HOOK_LAG_MS: u64 = 1000;

/// Consecutive idle scrapes required before demoting a busy pane, to ride out spinner redraws.
const IDLE_SCRAPES_TO_DEMOTE: u8 = 2;

#[derive(Debug, Default)]
pub struct Store {
    panes: BTreeMap<String, PaneRecord>,
}

impl Store {
    pub fn records(&self) -> Vec<PaneRecord> {
        self.panes.values().cloned().collect()
    }

    pub fn get(&self, pane: &str) -> Option<&PaneRecord> {
        self.panes.get(pane)
    }

    pub fn apply(
        &mut self,
        pane: &str,
        agent: Agent,
        parsed: &Parsed,
        now: u64,
        viewed: bool,
    ) -> Option<Transition> {
        if parsed.event == AgentEvent::SessionEnd {
            self.panes.remove(pane);
            return None;
        }
        let created = !self.panes.contains_key(pane);
        let rec = self
            .panes
            .entry(pane.to_owned())
            .or_insert_with(|| PaneRecord::new(pane, agent, State::Unknown, now));
        rec.agent = agent;
        // A hook that changes nothing (e.g. a subagent's) must not outrank the live status.
        if parsed.event != AgentEvent::NoChange {
            rec.last_event_ms = now;
            rec.idle_scrapes = 0;
        }
        if parsed.meta.session_id.is_some() {
            rec.session_id.clone_from(&parsed.meta.session_id);
        }
        if parsed.meta.cwd.is_some() {
            rec.cwd.clone_from(&parsed.meta.cwd);
        }
        for (field, new) in [
            (&mut rec.model, &parsed.meta.model),
            (&mut rec.effort, &parsed.meta.effort),
            (&mut rec.transcript, &parsed.meta.transcript),
        ] {
            if new.is_some() {
                field.clone_from(new);
            }
        }
        let next = match &parsed.event {
            AgentEvent::SessionStart => Some(State::Idle),
            AgentEvent::PromptSubmit => Some(State::Working),
            AgentEvent::PreTool { tool } => {
                rec.pending_tool.clone_from(tool);
                Some(State::Working)
            }
            AgentEvent::PostTool => {
                rec.pending_tool = None;
                Some(State::Working)
            }
            AgentEvent::PermissionPrompt => Some(State::Blocked),
            AgentEvent::IdlePrompt => (rec.state != State::Done).then_some(State::Idle),
            AgentEvent::Stop => {
                rec.pending_tool = None;
                Some(if viewed { State::Idle } else { State::Done })
            }
            AgentEvent::Unrecognized => Some(State::Unknown),
            AgentEvent::NoChange | AgentEvent::SessionEnd => None,
        };
        let from = (!created).then_some(rec.state);
        set_state(rec, next?, now, from)
    }

    pub fn reconcile(&mut self, snap: &Snapshot, now: u64) -> Vec<Transition> {
        let live: BTreeMap<&str, &PaneInfo> =
            snap.panes.iter().map(|p| (p.id.as_str(), p)).collect();
        self.panes.retain(|id, _| live.contains_key(id.as_str()));
        let mut out = Vec::new();
        for rec in self.panes.values_mut() {
            if let Some(info) = live.get(rec.pane.as_str()) {
                place(rec, info);
            }
            if rec.state == State::Done && snap.viewed.contains(&rec.pane) {
                let from = Some(rec.state);
                out.extend(set_state(rec, State::Idle, now, from));
            }
        }
        out
    }

    /// Inserts agent panes found in the snapshot that no hook has reported yet.
    pub fn recover(&mut self, snap: &Snapshot, now: u64) -> Vec<String> {
        let mut added = Vec::new();
        for info in snap.panes.iter().filter(|p| !p.is_sidebar) {
            if self.panes.contains_key(&info.id) {
                continue;
            }
            let Some(agent) = Agent::all()
                .iter()
                .copied()
                .find(|a| a.matches_process(&info.current_command))
            else {
                continue;
            };
            let mut rec = PaneRecord::new(&info.id, agent, State::Unknown, now);
            rec.cwd = Some(info.path.clone());
            place(&mut rec, info);
            self.panes.insert(info.id.clone(), rec);
            added.push(info.id.clone());
        }
        added
    }

    /// Applies each pane's live session status when it changed since the last call and is newer
    /// than the pane's last hook.
    pub fn apply_live(&mut self, live: &[Live], now: u64) -> Vec<Transition> {
        let mut out = Vec::new();
        for rec in self.panes.values_mut() {
            let Some(l) = live
                .iter()
                .filter(|l| l.pane == rec.pane && rec.session.as_deref() == Some(&l.session))
                .max_by_key(|l| l.updated_ms)
            else {
                rec.live_ms = None;
                continue;
            };
            if rec.live_ms.replace(l.updated_ms) == Some(l.updated_ms)
                || l.updated_ms + HOOK_LAG_MS < rec.last_event_ms
            {
                continue;
            }
            let next = match l.status {
                LiveStatus::Busy => State::Working,
                LiveStatus::Waiting => State::Blocked,
                // Only the Stop hook can tell a finished turn from an interrupted one.
                LiveStatus::Idle if rec.state == State::Done => continue,
                LiveStatus::Idle => State::Idle,
            };
            let from = Some(rec.state);
            out.extend(set_state(rec, next, now, from));
        }
        out
    }

    /// Panes whose state should be checked against the screen: busy or unknown, with no recent hook
    /// and no live session file.
    pub fn needs_scrape(&self, now: u64, threshold_ms: u64) -> Vec<(String, Agent)> {
        self.panes
            .values()
            .filter(|r| r.live_ms.is_none())
            .filter(|r| match r.state {
                State::Working | State::Blocked => {
                    now.saturating_sub(r.last_event_ms) >= threshold_ms
                }
                State::Unknown => true,
                State::Done | State::Idle => false,
            })
            .map(|r| (r.pane.clone(), r.agent))
            .collect()
    }

    pub fn apply_screen(&mut self, pane: &str, screen: Screen, now: u64) -> Option<Transition> {
        let rec = self.panes.get_mut(pane)?;
        if !matches!(screen, Screen::Idle | Screen::NoPrompt) {
            rec.idle_scrapes = 0;
        }
        let next = match (rec.state, screen) {
            (_, Screen::Unknown) | (State::Done | State::Idle, _) => None,
            (_, Screen::Blocked) => Some(State::Blocked),
            (_, Screen::Working) => Some(State::Working),
            (State::Unknown, Screen::Idle) => Some(State::Idle),
            (State::Working | State::Blocked, Screen::Idle) => {
                rec.idle_scrapes += 1;
                (rec.idle_scrapes >= IDLE_SCRAPES_TO_DEMOTE).then_some(State::Idle)
            }
            // The prompt was answered or dismissed; the turn's next hook says what happened.
            (State::Blocked, Screen::NoPrompt) => {
                rec.idle_scrapes += 1;
                (rec.idle_scrapes >= IDLE_SCRAPES_TO_DEMOTE).then_some(State::Working)
            }
            (State::Working | State::Unknown, Screen::NoPrompt) => None,
        };
        let from = Some(rec.state);
        set_state(rec, next?, now, from)
    }

    pub fn locate(&mut self, info: &PaneInfo) {
        if let Some(rec) = self.panes.get_mut(&info.id) {
            place(rec, info);
        }
    }

    /// Records the model and effort a pane last ran with; `None` keeps the previous value.
    pub fn set_model(&mut self, pane: &str, model: Option<String>, effort: Option<String>) -> bool {
        let Some(rec) = self.panes.get_mut(pane) else {
            return false;
        };
        let mut changed = false;
        for (field, new) in [(&mut rec.model, model), (&mut rec.effort, effort)] {
            if new.is_some() && *field != new {
                *field = new;
                changed = true;
            }
        }
        changed
    }

    pub fn set_branch(&mut self, cwd: &str, branch: Option<String>) -> bool {
        let mut changed = false;
        for rec in self.panes.values_mut() {
            if rec.cwd.as_deref() == Some(cwd) && rec.branch != branch {
                rec.branch.clone_from(&branch);
                changed = true;
            }
        }
        changed
    }
}

/// Copies where the pane lives in tmux onto its record.
fn place(rec: &mut PaneRecord, info: &PaneInfo) {
    rec.session = Some(info.session.clone());
    rec.window = Some(info.window);
    rec.window_id = Some(info.window_id.clone());
    rec.window_name = Some(info.window_name.clone());
    rec.active = info.active;
}

fn set_state(
    rec: &mut PaneRecord,
    next: State,
    now: u64,
    from: Option<State>,
) -> Option<Transition> {
    if from == Some(next) {
        return None;
    }
    rec.state = next;
    rec.since_ms = now;
    rec.idle_scrapes = 0;
    Some(Transition {
        pane: rec.pane.clone(),
        agent: rec.agent,
        from,
        to: next,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::claude_sessions::{Live, LiveStatus};
    use crate::hook::{Envelope, Meta};

    fn ev(event: AgentEvent) -> Parsed {
        Parsed {
            event,
            meta: Meta::default(),
        }
    }

    fn state(store: &Store, pane: &str) -> Option<State> {
        store.get(pane).map(|r| r.state)
    }

    #[test]
    fn hook_events_follow_the_table() {
        let cases = [
            (AgentEvent::SessionStart, State::Idle),
            (AgentEvent::PromptSubmit, State::Working),
            (AgentEvent::PreTool { tool: None }, State::Working),
            (AgentEvent::PermissionPrompt, State::Blocked),
            (AgentEvent::PostTool, State::Working),
            (AgentEvent::Stop, State::Done),
            (AgentEvent::Unrecognized, State::Unknown),
        ];
        for (event, want) in cases {
            let mut s = Store::default();
            s.apply("%1", Agent::Claude, &ev(AgentEvent::PromptSubmit), 1, false);
            s.apply("%1", Agent::Claude, &ev(event.clone()), 2, false);
            assert_eq!(state(&s, "%1"), Some(want), "{event:?}");
        }
    }

    #[test]
    fn transition_reports_change_only() {
        let mut s = Store::default();
        let t = s.apply("%1", Agent::Claude, &ev(AgentEvent::PromptSubmit), 5, false);
        assert_eq!(
            t,
            Some(Transition {
                pane: "%1".into(),
                agent: Agent::Claude,
                from: None,
                to: State::Working
            })
        );
        assert_eq!(
            s.apply("%1", Agent::Claude, &ev(AgentEvent::PostTool), 9, false),
            None
        );
        let rec = s.get("%1").unwrap();
        assert_eq!(rec.since_ms, 5);
        assert_eq!(rec.last_event_ms, 9);
    }

    #[test]
    fn session_end_removes_pane() {
        let mut s = Store::default();
        s.apply("%1", Agent::Claude, &ev(AgentEvent::SessionStart), 1, false);
        assert_eq!(
            s.apply("%1", Agent::Claude, &ev(AgentEvent::SessionEnd), 2, false),
            None
        );
        assert!(s.get("%1").is_none());
    }

    #[test]
    fn stop_while_viewed_goes_idle() {
        let mut s = Store::default();
        s.apply("%1", Agent::Claude, &ev(AgentEvent::PromptSubmit), 1, true);
        s.apply("%1", Agent::Claude, &ev(AgentEvent::Stop), 2, true);
        assert_eq!(state(&s, "%1"), Some(State::Idle));
    }

    #[test]
    fn subagent_tool_use_after_stop_keeps_done() {
        let mut s = Store::default();
        s.apply("%1", Agent::Claude, &ev(AgentEvent::PromptSubmit), 1, false);
        s.apply("%1", Agent::Claude, &ev(AgentEvent::Stop), 2, false);
        let sub = serde_json::json!({"tool_name": "Read", "agent_id": "a1"});
        let parsed = Agent::Claude.parse("PreToolUse", &sub);
        assert_eq!(s.apply("%1", Agent::Claude, &parsed, 3, false), None);
        assert_eq!(state(&s, "%1"), Some(State::Done));
    }

    #[test]
    fn pending_tool_tracks_pre_and_post() {
        let mut s = Store::default();
        let pre = ev(AgentEvent::PreTool {
            tool: Some("Write".into()),
        });
        s.apply("%1", Agent::Claude, &pre, 1, false);
        assert_eq!(s.get("%1").unwrap().pending_tool.as_deref(), Some("Write"));
        s.apply("%1", Agent::Claude, &ev(AgentEvent::PostTool), 2, false);
        assert_eq!(s.get("%1").unwrap().pending_tool, None);
    }

    #[test]
    fn idle_prompt_clears_busy_but_not_done() {
        let mut s = Store::default();
        s.apply("%1", Agent::Claude, &ev(AgentEvent::PromptSubmit), 1, false);
        s.apply("%1", Agent::Claude, &ev(AgentEvent::IdlePrompt), 2, false);
        assert_eq!(state(&s, "%1"), Some(State::Idle));
        s.apply("%1", Agent::Claude, &ev(AgentEvent::Stop), 3, false);
        s.apply("%1", Agent::Claude, &ev(AgentEvent::IdlePrompt), 4, false);
        assert_eq!(state(&s, "%1"), Some(State::Done));
    }

    #[test]
    fn meta_is_recorded() {
        let mut s = Store::default();
        let p = Parsed {
            event: AgentEvent::SessionStart,
            meta: Meta {
                session_id: Some("abc".into()),
                cwd: Some("/w".into()),
                model: Some("claude-opus-5-5".into()),
                effort: Some("high".into()),
                transcript: Some("/t.jsonl".into()),
            },
        };
        s.apply("%1", Agent::Claude, &p, 1, false);
        s.apply("%1", Agent::Claude, &ev(AgentEvent::Stop), 2, false);
        let rec = s.get("%1").unwrap();
        assert_eq!(rec.session_id.as_deref(), Some("abc"));
        assert_eq!(rec.cwd.as_deref(), Some("/w"));
        assert_eq!(rec.model.as_deref(), Some("claude-opus-5-5"));
        assert_eq!(rec.effort.as_deref(), Some("high"));
        assert_eq!(rec.transcript.as_deref(), Some("/t.jsonl"));
        assert!(s.set_model("%1", Some("claude-sonnet-5-5".into()), Some("low".into())));
        assert!(!s.set_model("%1", Some("claude-sonnet-5-5".into()), Some("low".into())));
        assert!(
            !s.set_model("%1", None, None),
            "unknown values keep the last known ones"
        );
        assert!(!s.set_model("%9", Some("x".into()), None));
        let rec = s.get("%1").unwrap();
        assert_eq!(rec.model.as_deref(), Some("claude-sonnet-5-5"));
        assert_eq!(rec.effort.as_deref(), Some("low"));
    }

    fn pane(id: &str, cmd: &str) -> PaneInfo {
        PaneInfo {
            id: id.into(),
            session: "main".into(),
            window: 2,
            window_id: "@2".into(),
            current_command: cmd.into(),
            path: "/w".into(),
            is_sidebar: false,
            window_panes: 1,
            active: true,
            window_name: "api".into(),
        }
    }

    #[test]
    fn reconcile_drops_vanished_and_clears_viewed_done() {
        let mut s = Store::default();
        s.apply("%1", Agent::Claude, &ev(AgentEvent::Stop), 1, false);
        s.apply("%2", Agent::Claude, &ev(AgentEvent::Stop), 1, false);
        s.apply("%3", Agent::Claude, &ev(AgentEvent::Stop), 1, false);
        let snap = Snapshot {
            panes: vec![pane("%1", "claude"), pane("%2", "claude")],
            viewed: HashSet::from(["%1".to_string()]),
        };
        let ts = s.reconcile(&snap, 7);
        assert_eq!(ts.len(), 1);
        assert_eq!(ts[0].to, State::Idle);
        assert_eq!(state(&s, "%1"), Some(State::Idle));
        assert_eq!(state(&s, "%2"), Some(State::Done));
        assert!(s.get("%3").is_none());
        let rec = s.get("%2").unwrap();
        assert_eq!(rec.session.as_deref(), Some("main"));
        assert_eq!(rec.window, Some(2));
        assert_eq!(rec.window_id.as_deref(), Some("@2"));
        assert_eq!(rec.window_name.as_deref(), Some("api"));
        assert!(rec.active);
    }

    #[test]
    fn recover_inserts_untracked_agents_as_unknown() {
        let mut s = Store::default();
        s.apply("%1", Agent::Claude, &ev(AgentEvent::Stop), 1, false);
        let mut side = pane("%4", "2.1.307");
        side.is_sidebar = true;
        let snap = Snapshot {
            panes: vec![
                pane("%1", "claude"),
                pane("%2", "2.1.307"),
                pane("%3", "zsh"),
                side,
            ],
            viewed: HashSet::new(),
        };
        assert_eq!(s.recover(&snap, 5), vec!["%2".to_string()]);
        assert_eq!(state(&s, "%1"), Some(State::Done));
        assert_eq!(state(&s, "%2"), Some(State::Unknown));
        assert_eq!(s.get("%2").unwrap().cwd.as_deref(), Some("/w"));
    }

    #[test]
    fn scrape_selection_respects_threshold() {
        let mut s = Store::default();
        s.apply(
            "%1",
            Agent::Claude,
            &ev(AgentEvent::PromptSubmit),
            1000,
            false,
        );
        s.apply("%2", Agent::Claude, &ev(AgentEvent::Stop), 1000, false);
        s.apply(
            "%3",
            Agent::Claude,
            &ev(AgentEvent::PermissionPrompt),
            1000,
            false,
        );
        assert!(s.needs_scrape(2000, 1500).is_empty());
        let mut got = s.needs_scrape(2600, 1500);
        got.sort();
        assert_eq!(
            got,
            vec![
                ("%1".to_string(), Agent::Claude),
                ("%3".to_string(), Agent::Claude)
            ]
        );
    }

    #[test]
    fn idle_streak_requires_consecutive_scrapes() {
        let mut s = Store::default();
        s.apply(
            "%1",
            Agent::Claude,
            &ev(AgentEvent::PermissionPrompt),
            1,
            false,
        );
        s.apply_screen("%1", Screen::Idle, 2);
        s.apply_screen("%1", Screen::Blocked, 3);
        assert_eq!(s.apply_screen("%1", Screen::Idle, 4), None);
        assert_eq!(state(&s, "%1"), Some(State::Blocked));
        s.apply("%2", Agent::Claude, &ev(AgentEvent::PromptSubmit), 1, false);
        s.apply_screen("%2", Screen::Idle, 2);
        s.apply_screen("%2", Screen::Unknown, 3);
        assert_eq!(s.apply_screen("%2", Screen::Idle, 4), None);
        assert_eq!(state(&s, "%2"), Some(State::Working));
    }

    #[test]
    fn screen_corrects_busy_states() {
        let mut s = Store::default();
        s.apply(
            "%1",
            Agent::Claude,
            &ev(AgentEvent::PreTool { tool: None }),
            1,
            false,
        );
        assert_eq!(
            s.apply_screen("%1", Screen::Blocked, 2).map(|t| t.to),
            Some(State::Blocked)
        );
        assert_eq!(s.apply_screen("%1", Screen::Blocked, 3), None);
        assert_eq!(s.apply_screen("%1", Screen::Idle, 4), None);
        assert_eq!(state(&s, "%1"), Some(State::Blocked));
        assert_eq!(
            s.apply_screen("%1", Screen::Idle, 5).map(|t| t.to),
            Some(State::Idle)
        );
        assert_eq!(s.apply_screen("%1", Screen::Working, 6), None);
    }

    #[test]
    fn missing_prompt_clears_blocked_after_a_streak() {
        let mut s = Store::default();
        s.apply(
            "%1",
            Agent::Kiro,
            &ev(AgentEvent::PreTool { tool: None }),
            1,
            false,
        );
        assert_eq!(
            s.apply_screen("%1", Screen::Blocked, 2).map(|t| t.to),
            Some(State::Blocked)
        );
        // Approved, denied, or interrupted: none of these sends a Kiro hook.
        assert_eq!(s.apply_screen("%1", Screen::NoPrompt, 3), None);
        s.apply_screen("%1", Screen::Blocked, 4);
        assert_eq!(
            s.apply_screen("%1", Screen::NoPrompt, 5),
            None,
            "streak restarts"
        );
        assert_eq!(
            s.apply_screen("%1", Screen::NoPrompt, 6).map(|t| t.to),
            Some(State::Working)
        );
    }

    #[test]
    fn missing_prompt_tells_nothing_about_other_states() {
        let mut s = Store::default();
        s.apply("%1", Agent::Kiro, &ev(AgentEvent::PromptSubmit), 1, false);
        let snap = Snapshot {
            panes: vec![pane("%2", "kiro-cli")],
            viewed: HashSet::new(),
        };
        s.recover(&snap, 1);
        for at in 2..6 {
            assert_eq!(s.apply_screen("%1", Screen::NoPrompt, at), None);
            assert_eq!(s.apply_screen("%2", Screen::NoPrompt, at), None);
        }
        assert_eq!(state(&s, "%1"), Some(State::Working));
        assert_eq!(state(&s, "%2"), Some(State::Unknown));
    }

    #[test]
    fn working_screen_resets_idle_streak() {
        let mut s = Store::default();
        s.apply("%1", Agent::Claude, &ev(AgentEvent::PromptSubmit), 1, false);
        s.apply_screen("%1", Screen::Idle, 2);
        s.apply_screen("%1", Screen::Working, 3);
        assert_eq!(s.apply_screen("%1", Screen::Idle, 4), None);
        assert_eq!(state(&s, "%1"), Some(State::Working));
    }

    #[test]
    fn unknown_resolves_from_first_screen() {
        let mut s = Store::default();
        let snap = Snapshot {
            panes: vec![pane("%2", "claude")],
            viewed: HashSet::new(),
        };
        s.recover(&snap, 1);
        assert_eq!(
            s.needs_scrape(1, 1500),
            vec![("%2".to_string(), Agent::Claude)]
        );
        assert_eq!(
            s.apply_screen("%2", Screen::Idle, 2).map(|t| t.to),
            Some(State::Idle)
        );
        assert!(s.needs_scrape(10_000, 1500).is_empty());
        assert_eq!(s.apply_screen("%2", Screen::Unknown, 3), None);
    }

    #[test]
    fn hook_event_resets_scrape_timer() {
        let mut s = Store::default();
        s.apply(
            "%1",
            Agent::Claude,
            &ev(AgentEvent::PromptSubmit),
            1000,
            false,
        );
        s.apply("%1", Agent::Claude, &ev(AgentEvent::PostTool), 2400, false);
        assert!(s.needs_scrape(2600, 1500).is_empty());
    }

    #[test]
    fn locate_copies_session_and_window() {
        let mut s = Store::default();
        s.apply("%1", Agent::Claude, &ev(AgentEvent::Stop), 1, false);
        s.locate(&pane("%1", "claude"));
        s.locate(&pane("%9", "claude"));
        let rec = s.get("%1").unwrap();
        assert_eq!(
            (rec.session.as_deref(), rec.window),
            (Some("main"), Some(2))
        );
        assert_eq!(rec.window_name.as_deref(), Some("api"));
        assert!(s.get("%9").is_none());
    }

    #[test]
    fn set_branch_updates_matching_cwd() {
        let mut s = Store::default();
        let p = Parsed {
            event: AgentEvent::SessionStart,
            meta: Meta {
                cwd: Some("/w".into()),
                ..Meta::default()
            },
        };
        s.apply("%1", Agent::Claude, &p, 1, false);
        assert!(s.set_branch("/w", Some("main".into())));
        assert!(!s.set_branch("/w", Some("main".into())));
        assert!(!s.set_branch("/other", Some("x".into())));
        assert_eq!(s.get("%1").unwrap().branch.as_deref(), Some("main"));
    }

    fn live(status: LiveStatus, updated_ms: u64) -> Live {
        Live {
            pane: "%1".into(),
            session: "main".into(),
            status,
            updated_ms,
        }
    }

    /// A pane placed in session `main` whose last hook arrived at `at`.
    fn placed(event: AgentEvent, at: u64) -> Store {
        let mut s = Store::default();
        s.apply("%1", Agent::Claude, &ev(event), at, false);
        s.locate(&pane("%1", "claude"));
        s
    }

    #[test]
    fn live_status_maps_to_states() {
        let mut s = placed(AgentEvent::SessionStart, 1);
        let t = s.apply_live(&[live(LiveStatus::Busy, 10)], 11);
        assert_eq!(t.iter().map(|t| t.to).collect::<Vec<_>>(), [State::Working]);
        s.apply_live(&[live(LiveStatus::Waiting, 20)], 21);
        assert_eq!(state(&s, "%1"), Some(State::Blocked));
        s.apply_live(&[live(LiveStatus::Idle, 30)], 31);
        assert_eq!(state(&s, "%1"), Some(State::Idle));
        assert_eq!(s.get("%1").unwrap().since_ms, 31);
    }

    #[test]
    fn unchanged_live_status_does_not_override_hooks() {
        let mut s = placed(AgentEvent::SessionStart, 1);
        s.apply_live(&[live(LiveStatus::Idle, 5)], 6);
        s.apply("%1", Agent::Claude, &ev(AgentEvent::PromptSubmit), 7, false);
        assert!(s.apply_live(&[live(LiveStatus::Idle, 5)], 8).is_empty());
        assert_eq!(state(&s, "%1"), Some(State::Working));
    }

    #[test]
    fn live_status_older_than_last_hook_is_ignored() {
        let mut s = placed(AgentEvent::PromptSubmit, 5000);
        assert!(
            s.apply_live(&[live(LiveStatus::Idle, 3000)], 5001)
                .is_empty()
        );
        assert_eq!(state(&s, "%1"), Some(State::Working));
    }

    #[test]
    fn live_status_just_before_a_late_hook_still_counts() {
        // Esc right after a prompt: the hook reaches the daemon after Claude wrote `idle`.
        let mut s = placed(AgentEvent::PromptSubmit, 5300);
        s.apply_live(&[live(LiveStatus::Idle, 5200)], 5301);
        assert_eq!(state(&s, "%1"), Some(State::Idle));
    }

    #[test]
    fn newest_live_entry_for_a_pane_wins() {
        let mut s = placed(AgentEvent::SessionStart, 1);
        // A crashed Claude's file can outlive it next to the new one's.
        s.apply_live(
            &[live(LiveStatus::Busy, 10), live(LiveStatus::Waiting, 20)],
            21,
        );
        assert_eq!(state(&s, "%1"), Some(State::Blocked));
        s.apply_live(
            &[live(LiveStatus::Waiting, 20), live(LiveStatus::Busy, 10)],
            22,
        );
        assert_eq!(state(&s, "%1"), Some(State::Blocked));
    }

    #[test]
    fn live_idle_keeps_unseen_done() {
        let mut s = placed(AgentEvent::PromptSubmit, 1);
        s.apply("%1", Agent::Claude, &ev(AgentEvent::Stop), 2, false);
        assert!(s.apply_live(&[live(LiveStatus::Idle, 3)], 4).is_empty());
        assert_eq!(state(&s, "%1"), Some(State::Done));
    }

    #[test]
    fn live_status_from_another_session_is_ignored() {
        let mut s = placed(AgentEvent::SessionStart, 1);
        let mut other = live(LiveStatus::Busy, 10);
        other.session = "elsewhere".into();
        assert!(s.apply_live(&[other], 11).is_empty());
        assert_eq!(state(&s, "%1"), Some(State::Idle));
    }

    #[test]
    fn live_panes_are_not_scraped_until_their_file_goes() {
        let mut s = placed(AgentEvent::PromptSubmit, 1000);
        s.apply_live(&[live(LiveStatus::Busy, 1001)], 1002);
        assert!(s.needs_scrape(9000, 1500).is_empty());
        s.apply_live(&[], 9001);
        assert_eq!(
            s.needs_scrape(9000, 1500),
            vec![("%1".to_string(), Agent::Claude)]
        );
    }

    #[test]
    fn live_status_resolves_recovered_panes() {
        let mut s = Store::default();
        let snap = Snapshot {
            panes: vec![pane("%1", "claude")],
            viewed: HashSet::new(),
        };
        s.recover(&snap, 1);
        s.apply_live(&[live(LiveStatus::Busy, 0)], 2);
        assert_eq!(state(&s, "%1"), Some(State::Working));
    }

    #[test]
    fn no_change_hooks_do_not_count_as_news() {
        let mut s = placed(AgentEvent::PromptSubmit, 1);
        s.apply("%1", Agent::Claude, &ev(AgentEvent::NoChange), 9, false);
        assert_eq!(s.get("%1").unwrap().last_event_ms, 1);
        s.apply_live(&[live(LiveStatus::Idle, 5)], 10);
        assert_eq!(state(&s, "%1"), Some(State::Idle));
    }

    /// Replays the recorded calibration session and checks the state after each event.
    #[test]
    fn replays_recorded_session() {
        let path =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/claude/session.jsonl");
        let text = std::fs::read_to_string(&path).unwrap();
        let mut s = Store::default();
        let mut seen = Vec::new();
        for (i, line) in text.lines().enumerate() {
            let env: Envelope = serde_json::from_str(line).unwrap();
            let agent = Agent::from_name(&env.agent).unwrap();
            let pane = env.pane.clone().unwrap();
            let parsed = agent.parse(&env.event, &crate::hook::slim_payload(&env.payload));
            s.apply(&pane, agent, &parsed, i as u64, false);
            seen.push(state(&s, &pane).map_or("gone", State::name));
        }
        assert_eq!(
            seen,
            vec![
                "idle",    // SessionStart
                "working", // prompt: write hello.txt
                "working", // PreToolUse Write
                "blocked", // permission_prompt
                "working", // approved: PostToolUse
                "done",    // Stop
                "working", // prompt: write bye.txt
                "working", // PreToolUse, denied with 3 (no hook; scrape corrects)
                "working", // prompt: write bye2.txt
                "working", // PreToolUse
                "blocked", // permission_prompt, then Escape (no hook)
                "working", // essay prompt, interrupted (no hook)
                "working", // prompt: sleep 12
                "working", "working", "done", "working", // prompt: sleep 20
                "working", "working", "done",
                "done", // idle_prompt does not clear an unseen done
                "gone", // SessionEnd
            ]
        );
    }

    /// Replays the synthetic Kiro session (see `fixtures/CALIBRATION.md`).
    #[test]
    fn replays_kiro_session() {
        let path =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/kiro/session.jsonl");
        let text = std::fs::read_to_string(&path).unwrap();
        let mut s = Store::default();
        let mut seen = Vec::new();
        for (i, line) in text.lines().enumerate() {
            let env: Envelope = serde_json::from_str(line).unwrap();
            let agent = Agent::from_name(&env.agent).unwrap();
            assert_eq!(agent, Agent::Kiro);
            let pane = env.pane.clone().unwrap();
            let parsed = agent.parse(&env.event, &crate::hook::slim_payload(&env.payload));
            s.apply(&pane, agent, &parsed, i as u64, false);
            seen.push(state(&s, &pane).map_or("gone", State::name));
        }
        assert_eq!(
            seen,
            vec![
                "idle",    // SessionStart
                "working", // prompt
                "working", // PreToolUse fs_write
                "working", // PostToolUse
                "done",    // Stop
                "working", // prompt
                "working", // PreToolUse, then Esc (no hook)
                "working", // next prompt
                "done",    // Stop
                "gone",    // SessionEnd
            ]
        );
    }
}
