use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::protocol::{Run, RunState};
use crate::tmux::{PaneInfo, Snapshot};

/// What `pane_current_command` reads while a shell waits at its prompt. A login shell shows
/// with a leading `-`.
const SHELLS: &[&str] = &[
    "sh", "bash", "zsh", "fish", "dash", "ksh", "mksh", "tcsh", "csh", "ash", "yash", "nu",
    "xonsh", "elvish", "pwsh",
];

pub fn is_shell(command: &str) -> bool {
    SHELLS.contains(&command.trim_start_matches('-'))
}

/// The shell a pane runs, by the name `pane_current_command` shows for it at its prompt.
/// `launched` is what started the pane (see `PaneInfo::launched`). `None` when that ran no
/// shell, or `sh`, which on macOS re-executes as bash, so its name says nothing.
pub fn pane_shell(launched: &str) -> Option<&str> {
    // The first shell word gets past `VAR=value`, `exec`, and wrappers such as `env` or
    // `reattach-to-user-namespace -l zsh`.
    let name = launched
        .trim_matches('"')
        .split_whitespace()
        .map(|w| w.rsplit('/').next().unwrap_or(w))
        .find(|w| is_shell(w))?
        .trim_start_matches('-');
    (name != "sh").then_some(name)
}

/// Whether `command` is the pane's shell waiting at its prompt. Without a known shell, any
/// shell name counts, so a script run by a shell-named interpreter there reads as idle.
fn at_prompt(command: &str, shell: Option<&str>) -> bool {
    let command = command.trim_start_matches('-');
    match shell {
        Some(shell) => command == shell,
        None => is_shell(command),
    }
}

/// The foreground process of each plain pane, by pane id.
#[derive(Debug, Default)]
pub struct Runs {
    by_pane: HashMap<String, Run>,
    /// When a snapshot was last read.
    checked: Option<Instant>,
    /// When each plain pane was first seen, in ms; `None` before the first update. Panes there
    /// at the first update map to `None`: settled, so they are judged at once.
    first_seen: Option<HashMap<String, Option<u64>>>,
}

/// How long a new pane goes unjudged. Its first instants can show tmux's own fork before it
/// execs, then the default shell tmux runs the pane's command through before that execs it;
/// neither ran anything, and several looks can land in that time.
const SETTLE_MS: u64 = 1000;

impl Runs {
    /// Whether a hidden-sidebar check is due, `every` after the last one.
    pub fn due(&self, now: Instant, every: Duration) -> bool {
        check_due(self.checked, now, every)
    }

    pub fn mark_checked(&mut self, at: Instant) {
        self.checked = Some(at);
    }

    pub fn get(&self, pane: &str) -> Option<&Run> {
        self.by_pane.get(pane)
    }

    /// Reads each plain pane's foreground command from `snap`, skipping sidebars and panes
    /// `is_agent` claims. A process that ends while its pane isn't viewed stays `Finished`
    /// until it is.
    pub fn update(
        &mut self,
        snap: &Snapshot,
        is_agent: impl Fn(&PaneInfo) -> bool,
        interactive: &[String],
        now: u64,
    ) {
        let mut next = HashMap::new();
        let mut first_seen = HashMap::new();
        for p in snap.panes.iter().filter(|p| !p.is_sidebar && !is_agent(p)) {
            let born = match &self.first_seen {
                Some(seen) => seen.get(&p.id).copied().unwrap_or(Some(now)),
                None => None,
            };
            first_seen.insert(p.id.clone(), born);
            if born.is_some_and(|b| now.saturating_sub(b) < SETTLE_MS) {
                continue;
            }
            let viewed = snap.viewed.contains(&p.id);
            let prev = self.by_pane.get(&p.id).map(|r| r.state);
            let shell = pane_shell(&p.launched);
            let state = if at_prompt(&p.current_command, shell) {
                match prev {
                    Some(RunState::Running | RunState::Interactive) if !viewed => {
                        RunState::Finished
                    }
                    Some(RunState::Finished) if !viewed => RunState::Finished,
                    _ => continue,
                }
            } else if interactive.contains(&p.current_command) {
                RunState::Interactive
            } else {
                RunState::Running
            };
            let since_ms = match self.by_pane.get(&p.id) {
                Some(r) if r.state == state => r.since_ms,
                _ => now,
            };
            next.insert(p.id.clone(), Run { state, since_ms });
        }
        self.by_pane = next;
        self.first_seen = Some(first_seen);
    }
}

/// How early a check may run. A check is marked when its snapshot returns, partway into the
/// tick that ran it, so the tick its interval ends on comes slightly short of the interval.
const SLACK: Duration = Duration::from_millis(super::TICK.as_millis() as u64 / 2);

/// Whether a hidden-sidebar process check is due, `every` after the last look at `last`.
fn check_due(last: Option<Instant>, now: Instant, every: Duration) -> bool {
    last.is_none_or(|t| now.duration_since(t) + SLACK >= every)
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    fn pane(id: &str, cmd: &str) -> PaneInfo {
        PaneInfo {
            id: id.into(),
            current_command: cmd.into(),
            ..PaneInfo::default()
        }
    }

    fn snap(panes: &[(&str, &str)], viewed: &[&str]) -> Snapshot {
        Snapshot {
            panes: panes.iter().map(|&(id, cmd)| pane(id, cmd)).collect(),
            viewed: viewed.iter().map(|&v| v.to_owned()).collect::<HashSet<_>>(),
        }
    }

    fn interactive() -> Vec<String> {
        vec!["hx".into(), "less".into()]
    }

    /// Applies `snap` at `now`, with no agents. A pane with no `launched` has no known shell,
    /// so any shell name reads as its prompt.
    fn step(runs: &mut Runs, snap: &Snapshot, now: u64) {
        runs.update(snap, |_| false, &interactive(), now);
    }

    fn state(runs: &Runs, pane: &str) -> Option<RunState> {
        runs.get(pane).map(|r| r.state)
    }

    #[test]
    fn shells_are_recognized_with_or_without_login_dash() {
        assert!(is_shell("zsh"));
        assert!(is_shell("-zsh"));
        assert!(is_shell("bash"));
        assert!(!is_shell("cargo"));
        assert!(!is_shell("zshell"));
    }

    #[test]
    fn idle_shell_has_no_run() {
        let mut r = Runs::default();
        step(&mut r, &snap(&[("%1", "zsh"), ("%2", "-bash")], &[]), 1);
        assert_eq!(state(&r, "%1"), None);
        assert_eq!(state(&r, "%2"), None);
    }

    #[test]
    fn unviewed_process_that_ends_is_finished_until_viewed() {
        let mut r = Runs::default();
        step(&mut r, &snap(&[("%1", "cargo")], &[]), 1);
        assert_eq!(
            r.get("%1"),
            Some(&Run {
                state: RunState::Running,
                since_ms: 1
            })
        );
        step(&mut r, &snap(&[("%1", "zsh")], &[]), 5);
        assert_eq!(
            r.get("%1"),
            Some(&Run {
                state: RunState::Finished,
                since_ms: 5
            })
        );
        step(&mut r, &snap(&[("%1", "zsh")], &[]), 6);
        assert_eq!(state(&r, "%1"), Some(RunState::Finished), "still unseen");
        step(&mut r, &snap(&[("%1", "zsh")], &["%1"]), 7);
        assert_eq!(state(&r, "%1"), None);
    }

    #[test]
    fn process_that_ends_while_viewed_leaves_nothing() {
        let mut r = Runs::default();
        step(&mut r, &snap(&[("%1", "sleep")], &[]), 1);
        step(&mut r, &snap(&[("%1", "zsh")], &["%1"]), 2);
        assert_eq!(state(&r, "%1"), None);
    }

    #[test]
    fn new_process_after_finish_runs_from_its_own_start() {
        let mut r = Runs::default();
        step(&mut r, &snap(&[("%1", "make")], &[]), 1);
        step(&mut r, &snap(&[("%1", "zsh")], &[]), 2);
        step(&mut r, &snap(&[("%1", "make")], &[]), 9);
        assert_eq!(
            r.get("%1"),
            Some(&Run {
                state: RunState::Running,
                since_ms: 9
            })
        );
    }

    #[test]
    fn running_keeps_its_start_across_command_names() {
        let mut r = Runs::default();
        step(&mut r, &snap(&[("%1", "npm")], &[]), 1);
        step(&mut r, &snap(&[("%1", "node")], &["%1"]), 4);
        assert_eq!(
            r.get("%1"),
            Some(&Run {
                state: RunState::Running,
                since_ms: 1
            })
        );
    }

    #[test]
    fn interactive_programs_are_their_own_state() {
        let mut r = Runs::default();
        step(&mut r, &snap(&[("%1", "hx"), ("%2", "less")], &[]), 1);
        step(&mut r, &snap(&[("%1", "hx"), ("%2", "less")], &[]), 3);
        assert_eq!(
            r.get("%1"),
            Some(&Run {
                state: RunState::Interactive,
                since_ms: 1
            })
        );
        // Quit while looking: nothing; dropped while away: finished.
        step(&mut r, &snap(&[("%1", "zsh"), ("%2", "zsh")], &["%1"]), 4);
        assert_eq!(state(&r, "%1"), None);
        assert_eq!(state(&r, "%2"), Some(RunState::Finished));
    }

    #[test]
    fn switching_between_running_and_interactive_restarts_the_clock() {
        let mut r = Runs::default();
        step(&mut r, &snap(&[("%1", "git")], &[]), 1);
        step(&mut r, &snap(&[("%1", "less")], &[]), 2);
        assert_eq!(
            r.get("%1"),
            Some(&Run {
                state: RunState::Interactive,
                since_ms: 2
            })
        );
    }

    #[test]
    fn sidebars_and_agents_are_skipped() {
        let mut r = Runs::default();
        let mut s = snap(&[("%1", "simul"), ("%2", "2.1.319"), ("%3", "cargo")], &[]);
        s.panes[0].is_sidebar = true;
        r.update(&s, |p| p.id == "%2", &interactive(), 1);
        assert_eq!(state(&r, "%1"), None);
        assert_eq!(state(&r, "%2"), None);
        assert_eq!(state(&r, "%3"), Some(RunState::Running));
        // A pane that turns into an agent drops its run.
        r.update(&s, |p| p.id != "%1", &interactive(), 2);
        assert_eq!(state(&r, "%3"), None);
    }

    #[test]
    fn vanished_panes_are_dropped() {
        let mut r = Runs::default();
        step(&mut r, &snap(&[("%1", "cargo")], &[]), 1);
        step(&mut r, &snap(&[], &[]), 2);
        assert_eq!(state(&r, "%1"), None);
        step(&mut r, &snap(&[("%1", "zsh")], &[]), 3);
        assert_eq!(state(&r, "%1"), None, "no finish for a pane id seen anew");
    }

    #[test]
    fn a_panes_shell_is_the_first_shell_in_what_launched_it() {
        assert_eq!(pane_shell("/bin/zsh"), Some("zsh"));
        assert_eq!(pane_shell("/opt/homebrew/bin/fish"), Some("fish"));
        assert_eq!(pane_shell("bash"), Some("bash"));
        assert_eq!(pane_shell("-zsh"), Some("zsh"), "as a login shell");
        // tmux quotes a start command that has spaces.
        assert_eq!(pane_shell("\"FOO=1 exec zsh -f\""), Some("zsh"));
        // Wrappers in default-command.
        assert_eq!(pane_shell("reattach-to-user-namespace -l zsh"), Some("zsh"));
        assert_eq!(pane_shell("/usr/bin/env fish"), Some("fish"));
        assert_eq!(pane_shell("\"sleep 600\""), None, "not a shell");
        assert_eq!(pane_shell("nvim ~/.zshrc"), None);
        assert_eq!(pane_shell("sh"), None, "sh hides behind another name");
        assert_eq!(pane_shell("/bin/sh -c make"), None);
        assert_eq!(pane_shell(""), None);
    }

    /// `snap` with every pane launched by `launched`.
    fn launched(mut s: Snapshot, launched: &str) -> Snapshot {
        for p in &mut s.panes {
            p.launched = launched.into();
        }
        s
    }

    #[test]
    fn script_under_another_shells_name_is_running() {
        // `./build.sh` in a zsh pane reads as `bash`.
        let mut r = Runs::default();
        let s = launched(
            snap(&[("%1", "bash"), ("%2", "zsh"), ("%3", "-zsh")], &[]),
            "/bin/zsh",
        );
        step(&mut r, &s, 1);
        assert_eq!(state(&r, "%1"), Some(RunState::Running));
        assert_eq!(state(&r, "%2"), None);
        assert_eq!(state(&r, "%3"), None, "a login shell");
        step(
            &mut r,
            &launched(snap(&[("%1", "zsh")], &[]), "/bin/zsh"),
            2,
        );
        assert_eq!(state(&r, "%1"), Some(RunState::Finished));
    }

    #[test]
    fn a_new_panes_launch_is_not_a_run() {
        // tmux forks itself, then runs `zsh -f` through its default shell, bash here, which
        // execs zsh. Looks in between must not read as a command that ran and ended.
        let mut r = Runs::default();
        step(
            &mut r,
            &launched(snap(&[("%1", "zsh")], &[]), "/bin/zsh"),
            1,
        );
        let both = |second: &str| {
            let mut s = launched(snap(&[("%1", "zsh"), ("%2", second)], &[]), "/bin/zsh");
            s.panes[1].launched = "\"zsh -f\"".into();
            s
        };
        step(&mut r, &both("tmux"), 100);
        step(&mut r, &both("bash"), 150);
        step(&mut r, &both("bash"), 100 + SETTLE_MS - 1);
        assert_eq!(state(&r, "%2"), None, "still settling");
        step(&mut r, &both("zsh"), 100 + SETTLE_MS);
        assert_eq!(state(&r, "%2"), None, "nothing ran, so nothing finished");
    }

    #[test]
    fn a_new_pane_is_judged_once_settled() {
        for (cmd, want) in [
            ("bash", RunState::Running),
            ("cargo", RunState::Running),
            ("hx", RunState::Interactive),
        ] {
            let s = launched(snap(&[("%9", cmd)], &[]), "/bin/zsh");
            let mut r = Runs::default();
            step(&mut r, &snap(&[], &[]), 1);
            step(&mut r, &s, 10);
            assert_eq!(state(&r, "%9"), None, "{cmd}");
            step(&mut r, &s, 10 + SETTLE_MS);
            assert_eq!(
                r.get("%9"),
                Some(&Run {
                    state: want,
                    since_ms: 10 + SETTLE_MS
                }),
                "{cmd}"
            );
        }
    }

    #[test]
    fn panes_there_at_the_first_look_are_judged_at_once() {
        let mut r = Runs::default();
        step(
            &mut r,
            &launched(snap(&[("%1", "cargo")], &[]), "/bin/zsh"),
            1,
        );
        assert_eq!(state(&r, "%1"), Some(RunState::Running));
    }

    #[test]
    fn each_pane_is_judged_by_its_own_shell() {
        let mut r = Runs::default();
        let mut s = snap(&[("%1", "bash"), ("%2", "zsh")], &[]);
        s.panes[0].launched = "/bin/bash".into();
        s.panes[1].launched = "\"exec bash\"".into();
        step(&mut r, &s, 1);
        assert_eq!(state(&r, "%1"), None, "bash at its own prompt");
        assert_eq!(
            state(&r, "%2"),
            Some(RunState::Running),
            "zsh run from bash"
        );
    }

    #[test]
    fn hidden_checks_wait_out_their_interval() {
        let t0 = Instant::now();
        let every = Duration::from_secs(10);
        assert!(check_due(None, t0, every), "first check runs at once");
        assert!(!check_due(Some(t0), t0 + Duration::from_secs(9), every));
        assert!(check_due(Some(t0), t0 + Duration::from_secs(10), every));
        assert!(
            check_due(Some(t0), t0, Duration::ZERO),
            "0 checks on every tick"
        );
    }

    #[test]
    fn checks_land_on_the_tick_their_interval_ends_on() {
        // A check is marked when its snapshot returns, a little after the tick that ran it.
        let t0 = Instant::now();
        let late = Duration::from_millis(50);
        let second = Duration::from_secs(1);
        assert!(
            check_due(Some(t0), t0 + second - late, second),
            "every second means every tick"
        );
        let ten = Duration::from_secs(10);
        assert!(check_due(Some(t0), t0 + ten - late, ten), "the 10th tick");
        assert!(
            !check_due(Some(t0), t0 + ten - second - late, ten),
            "not the 9th"
        );
        assert!(
            !check_due(Some(t0), t0 + Duration::from_millis(400), second),
            "not twice in one tick"
        );
    }

    #[test]
    fn runs_are_due_by_their_last_check() {
        let mut runs = Runs::default();
        let t0 = Instant::now();
        let every = Duration::from_secs(10);
        assert!(runs.due(t0, every), "never checked");
        runs.mark_checked(t0);
        assert!(!runs.due(t0 + Duration::from_secs(9), every));
        assert!(runs.due(t0 + every, every));
    }
}
