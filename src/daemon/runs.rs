use std::collections::{HashMap, HashSet};

use anyhow::{Context, Result};

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

/// Pids from `ps -o pid=,pgid=,tpgid=` whose terminal's foreground group isn't their own. A
/// tpgid of -1 or 0 means no controlling terminal.
pub fn parse_jobs(ps: &str) -> HashSet<u32> {
    ps.lines()
        .filter_map(|line| {
            let mut f = line.split_whitespace().map(str::parse::<i64>);
            let (Some(Ok(pid)), Some(Ok(pgid)), Some(Ok(tpgid))) = (f.next(), f.next(), f.next())
            else {
                return None;
            };
            (tpgid > 0 && tpgid != pgid).then_some(u32::try_from(pid).ok()?)
        })
        .collect()
}

/// Which of `pids`, pane shells, have handed their terminal to a job.
pub async fn shells_with_jobs(pids: &[u32]) -> Result<HashSet<u32>> {
    if pids.is_empty() {
        return Ok(HashSet::new());
    }
    let list = pids
        .iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(",");
    // ps exits non-zero when a pid has gone, and still lists the others.
    let out = tokio::process::Command::new("ps")
        .args(["-o", "pid=,pgid=,tpgid=", "-p", &list])
        .output()
        .await
        .with_context(|| format!("failed to run ps for pane shells {list}"))?;
    Ok(parse_jobs(&String::from_utf8_lossy(&out.stdout)))
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
    /// `jobs` holds the pane pids (see `shells_with_jobs`) whose shell has handed the
    /// terminal to a job, which tells a running script from a shell at its prompt.
    pub fn update(
        &mut self,
        snap: &Snapshot,
        is_agent: impl Fn(&PaneInfo) -> bool,
        interactive: &[String],
        jobs: &HashSet<u32>,
        now: u64,
    ) {
        let mut next = HashMap::new();
        for p in snap.panes.iter().filter(|p| !p.is_sidebar && !is_agent(p)) {
            let viewed = snap.viewed.contains(&p.id);
            let prev = self.by_pane.get(&p.id).map(|r| r.state);
            let state = if is_shell(&p.current_command) && !jobs.contains(&p.pid) {
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

    /// Applies `snap` at `now`, with no agents and every shell at its prompt.
    fn step(runs: &mut Runs, snap: &Snapshot, now: u64) {
        runs.update(snap, |_| false, &interactive(), &HashSet::new(), now);
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
        r.update(&s, |p| p.id == "%2", &interactive(), &HashSet::new(), 1);
        assert_eq!(state(&r, "%1"), None);
        assert_eq!(state(&r, "%2"), None);
        assert_eq!(state(&r, "%3"), Some(RunState::Running));
        // A pane that turns into an agent drops its run.
        r.update(&s, |p| p.id != "%1", &interactive(), &HashSet::new(), 2);
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
    fn shell_named_job_runs_while_its_shell_is_in_the_background() {
        // `./build.sh` reads as `bash`, but the pane's shell no longer owns the terminal.
        let mut r = Runs::default();
        let mut s = snap(&[("%1", "bash"), ("%2", "zsh")], &[]);
        s.panes[0].pid = 41;
        s.panes[1].pid = 42;
        r.update(&s, |_| false, &interactive(), &HashSet::from([41]), 1);
        assert_eq!(state(&r, "%1"), Some(RunState::Running));
        assert_eq!(state(&r, "%2"), None);
        r.update(&s, |_| false, &interactive(), &HashSet::new(), 2);
        assert_eq!(state(&r, "%1"), Some(RunState::Finished));
    }

    #[test]
    fn ps_output_names_shells_whose_terminal_belongs_to_another_group() {
        let out = "  97454 97454 97454\n  98275 98275 98301\n  500   500    -1\n garbage\n";
        assert_eq!(parse_jobs(out), HashSet::from([98275]));
    }
}
