use std::collections::HashMap;

use super::state::{PaneInfo, Snapshot};
use crate::protocol::{Run, RunState};

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
}

impl Runs {
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
        for p in snap.panes.iter().filter(|p| !p.is_sidebar && !is_agent(p)) {
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
    }
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
}
