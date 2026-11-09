//! Updating in place. A daemon running from a plugin checkout installs the release the
//! checkout's VERSION names and re-executes into it: at startup, whenever an update (TPM's
//! `git pull`) changes VERSION, and on a backoff after a failed install or restart. Loading the
//! plugin replaces a daemon of another build the same way.

use std::ffi::OsString;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use tokio::sync::mpsc;

use super::{Cmd, Daemon};
use crate::client;
use crate::log;
use crate::protocol::{Ack, Identity, Request};

/// How long `replace` waits for the old daemon to let go of its socket.
const REPLACE_WAIT: Duration = Duration::from_secs(5);

/// The plugin checkout that `exe` was installed into, as `<plugin>/bin/simul` next to the
/// plugin's VERSION and installer; `None` for any other binary, such as a cargo build.
pub fn plugin_dir(exe: &Path) -> Option<PathBuf> {
    let bin = exe.parent()?;
    if bin.file_name()? != "bin" {
        return None;
    }
    let plugin = bin.parent()?;
    (plugin.join("VERSION").is_file() && installer(plugin).is_file()).then(|| plugin.to_owned())
}

fn installer(plugin: &Path) -> PathBuf {
    plugin.join("scripts/install-binary.sh")
}

/// First wait before retrying a failed install or restart; each failure in a row doubles it.
const RETRY_FIRST: Duration = Duration::from_secs(60);
const RETRY_MAX: Duration = Duration::from_secs(3600);

/// The hidden `simul daemon` flag naming the version a daemon restarted into after installing
/// it. A flag rather than an environment variable, so the daemon's children don't inherit it.
pub const RESTARTED_FOR: &str = "--restarted-for";

/// What to do once an install ends.
#[derive(Debug, PartialEq, Eq)]
pub enum Next {
    /// Re-execute into `bin/simul`, installed as this version.
    Restart(String),
    /// The checkout moved on during the install; install this version before restarting.
    Install(String),
    /// Nothing usable was installed; look at the checkout again after `delay`, if `retry` is
    /// still the latest by then (see `Updater::retry_due`).
    RetryIn { delay: Duration, retry: u64 },
}

/// What the daemon knows about updating itself from its plugin checkout.
#[derive(Debug)]
pub struct Updater {
    /// The plugin checkout this binary was installed into; `None` for other builds.
    pub plugin: Option<PathBuf>,
    /// This process's version.
    running: String,
    /// The version an installer is fetching now.
    installing: Option<String>,
    /// The version this process put in `bin/simul` and hasn't restarted into yet.
    installed: Option<String>,
    /// A version whose release came back reporting another version; installing it again would
    /// only loop.
    skip: Option<String>,
    /// The version last tried, so a newly named one starts its backoff afresh.
    attempted: Option<String>,
    /// Failed installs and restarts in a row, for backoff.
    failures: u32,
    /// Numbers retries; starting an install or arming a newer retry makes older ones stale.
    retry: u64,
    /// A retry is armed for `attempted`; until it fires, events naming it again wait for it.
    backing_off: bool,
}

impl Updater {
    pub fn new(plugin: Option<PathBuf>, running: &str) -> Updater {
        Updater {
            plugin,
            running: running.to_owned(),
            installing: None,
            installed: None,
            skip: None,
            attempted: None,
            failures: 0,
            retry: 0,
            backing_off: false,
        }
    }

    /// Records that this process is the restart after installing `version`. Returns true if it
    /// reports another version, in which case installing `version` again would only loop.
    pub fn restarted_for(&mut self, version: &str) -> bool {
        let mismatch = version != self.running;
        if mismatch {
            self.skip = Some(version.to_owned());
        }
        mismatch
    }

    /// What to do now that the checkout names `wanted`: restart into a build already waiting
    /// in `bin/simul`, or install it. Nothing for the running version, one that came back
    /// mismatched, one an armed retry will try, or while an install runs.
    pub fn on_change(&mut self, wanted: Option<String>) -> Option<Next> {
        if self.installing.is_some() {
            return None;
        }
        let version = wanted.filter(|v| *v != self.running && self.skip.as_ref() != Some(v))?;
        if self.installed.as_ref() == Some(&version) {
            return Some(Next::Restart(version));
        }
        if self.attempted.as_ref() == Some(&version) {
            if self.backing_off {
                return None;
            }
        } else {
            self.failures = 0;
        }
        self.attempted = Some(version.clone());
        self.installing = Some(version.clone());
        self.backing_off = false;
        self.retry += 1;
        Some(Next::Install(version))
    }

    /// Settles the running install; `wanted` is what the checkout names now. A failure backs
    /// off, keeping any build an earlier install left in `bin/simul` for `on_change` to use.
    pub fn finished(&mut self, ok: bool, wanted: Option<String>) -> Next {
        match (self.installing.take(), ok) {
            (Some(done), true) => {
                self.installed = Some(done.clone());
                self.on_change(wanted).unwrap_or(Next::Restart(done))
            }
            _ => self.back_off(),
        }
    }

    /// The restart into `bin/simul` failed; installs again after a backoff.
    pub fn restart_failed(&mut self) -> Next {
        self.installed = None;
        self.back_off()
    }

    /// Whether retry `retry` is still the latest; if so, the next `on_change` may install.
    pub fn take_retry(&mut self, retry: u64) -> bool {
        let due = retry == self.retry && self.installing.is_none();
        if due {
            self.backing_off = false;
        }
        due
    }

    fn back_off(&mut self) -> Next {
        self.failures = self.failures.saturating_add(1);
        self.retry += 1;
        self.backing_off = true;
        Next::RetryIn {
            delay: retry_delay(self.failures),
            retry: self.retry,
        }
    }
}

/// How long to wait after the `failures`th failure in a row.
fn retry_delay(failures: u32) -> Duration {
    let doublings = failures.saturating_sub(1).min(16);
    (RETRY_FIRST * 2u32.pow(doublings)).min(RETRY_MAX)
}

/// The daemon's arguments for a restart as `version`: `args` with any earlier
/// `--restarted-for` dropped and the new one appended.
pub fn restart_args(args: &[OsString], version: &str) -> Vec<OsString> {
    let prefix = format!("{RESTARTED_FOR}=");
    let mut out = Vec::with_capacity(args.len() + 2);
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        if arg == RESTARTED_FOR {
            rest.next();
        } else if !arg.to_string_lossy().starts_with(&prefix) {
            out.push(arg.clone());
        }
    }
    out.extend([OsString::from(RESTARTED_FOR), OsString::from(version)]);
    out
}

/// The version the plugin checkout names; `None` if VERSION can't be read or is empty.
pub fn wanted_version(plugin: &Path) -> Option<String> {
    let text = std::fs::read_to_string(plugin.join("VERSION")).ok()?;
    let version = text.trim();
    (!version.is_empty()).then(|| version.to_owned())
}

/// Sends `Cmd::PluginChanged` whenever something in `plugin` named VERSION changes. The OS
/// reports the change, so nothing polls; watching stops when the watcher is dropped.
pub fn watch(plugin: &Path, tx: mpsc::Sender<Cmd>) -> Result<::notify::RecommendedWatcher> {
    use ::notify::{Event, RecursiveMode, Watcher};
    let mut watcher = ::notify::recommended_watcher(move |event: ::notify::Result<Event>| {
        match event {
            Ok(e)
                if e.paths
                    .iter()
                    .any(|p| p.file_name().is_some_and(|n| n == "VERSION")) =>
            {
                // This runs on the watcher's own thread, outside the runtime. A closed channel
                // means the daemon is exiting.
                let _ = tx.blocking_send(Cmd::PluginChanged);
            }
            Ok(_) => {}
            Err(e) => log!("plugin watch: {e}"),
        }
    })
    .context("failed to start watching the plugin")?;
    watcher
        .watch(plugin, RecursiveMode::NonRecursive)
        .with_context(|| format!("failed to watch {}", plugin.display()))?;
    Ok(watcher)
}

/// Runs the plugin's installer, which downloads the release VERSION names into `bin/simul` and
/// checks its sha256. The error is the installer's last line of stderr.
pub async fn install(plugin: &Path) -> Result<()> {
    let script = installer(plugin);
    let out = tokio::process::Command::new("bash")
        .arg(&script)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .output()
        .await
        .with_context(|| format!("failed to run {}", script.display()))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        bail!(
            "{}",
            stderr.trim().lines().last().unwrap_or("installer failed")
        );
    }
    Ok(())
}

/// Replaces this process with `exe`, keeping its arguments, and tells it which `version` it
/// was installed as. The daemon's socket is closed on exec, so the new image's `bind` finds it
/// stale and takes it over. Returns only on failure.
pub fn exec(exe: &Path, version: &str) -> anyhow::Error {
    use std::os::unix::process::CommandExt;
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    let err = std::process::Command::new(exe)
        .args(restart_args(&args, version))
        .exec();
    anyhow::Error::new(err).context(format!("failed to run {}", exe.display()))
}

/// Makes way for build `me` at `socket`: a daemon of another version or binary is asked to
/// exit. A daemon too old to say which build it is stays, since it can't be asked to leave.
pub fn replace(socket: &Path, me: &Identity) -> Result<()> {
    if UnixStream::connect(socket).is_err() {
        return Ok(());
    }
    let running: Identity = match client::request(socket, &Request::Identify) {
        Ok(id) => id,
        Err(e) => {
            log!(
                "leaving the running daemon, which predates in-place updates ({e:#}); \
                 restart it with: pkill -f 'simul daemon'"
            );
            return Ok(());
        }
    };
    if running == *me {
        return Ok(());
    }
    log!(
        "replacing daemon {} ({}) with {} ({})",
        running.version,
        running.exe,
        me.version,
        me.exe
    );
    let _: Ack = client::request(socket, &Request::Shutdown)?;
    let deadline = Instant::now() + REPLACE_WAIT;
    while UnixStream::connect(socket).is_ok() {
        if Instant::now() >= deadline {
            bail!("the daemon at {} did not exit", socket.display());
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    Ok(())
}

impl Daemon {
    /// Installs the release the plugin checkout names, or restarts into it if it is already
    /// in `bin/simul`, unless this build already is it.
    pub(super) fn on_plugin_changed(&mut self) {
        let Some(plugin) = self.updater.plugin.clone() else {
            return;
        };
        if let Some(next) = self.updater.on_change(wanted_version(&plugin)) {
            self.follow(plugin, next);
        }
    }

    fn start_install(&self, plugin: PathBuf, version: String) {
        log!("plugin names {version}; installing it");
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let result = install(&plugin).await.map_err(|e| format!("{e:#}"));
            // The daemon loop only stops at shutdown, when the install no longer matters.
            let _ = tx.send(Cmd::Installed { version, result }).await;
        });
    }

    /// Re-executes into the freshly installed binary, which takes over from tmux's state;
    /// first installs again if the checkout moved on, and retries later if the install failed.
    pub(super) fn on_installed(&mut self, version: &str, result: Result<(), String>) {
        let Some(plugin) = self.updater.plugin.clone() else {
            return;
        };
        match &result {
            Ok(()) => log!("installed {version}"),
            Err(e) => log!("failed to install {version}: {e}"),
        }
        let next = self
            .updater
            .finished(result.is_ok(), wanted_version(&plugin));
        self.follow(plugin, next);
    }

    fn follow(&mut self, plugin: PathBuf, next: Next) {
        match next {
            Next::RetryIn { delay, retry } => {
                log!("looking at the plugin again in {}s", delay.as_secs());
                let tx = self.tx.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(delay).await;
                    // The daemon loop only stops at shutdown, when the retry no longer matters.
                    let _ = tx.send(Cmd::RetryInstall(retry)).await;
                });
            }
            Next::Install(version) => self.start_install(plugin, version),
            Next::Restart(version) => {
                log!("restarting into {version} as {}", self.opts.exe.display());
                let e = exec(&self.opts.exe, &version);
                log!("{e:#}");
                let next = self.updater.restart_failed();
                self.follow(plugin, next);
            }
        }
    }

    /// A retry armed after a failure, unless something newer has happened since.
    pub(super) fn on_retry_install(&mut self, retry: u64) {
        if self.updater.take_retry(retry) {
            self.on_plugin_changed();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ver(v: &str) -> Option<String> {
        Some(v.to_owned())
    }

    fn running(version: &str) -> Updater {
        Updater::new(None, version)
    }

    fn install(v: &str) -> Option<Next> {
        Some(Next::Install(v.to_owned()))
    }

    fn restart(v: &str) -> Next {
        Next::Restart(v.to_owned())
    }

    fn delay(next: &Next) -> Duration {
        match next {
            Next::RetryIn { delay, .. } => *delay,
            other => panic!("expected a retry, got {other:?}"),
        }
    }

    fn retry(next: &Next) -> u64 {
        match next {
            Next::RetryIn { retry, .. } => *retry,
            other => panic!("expected a retry, got {other:?}"),
        }
    }

    /// Fails the running install, lets its retry fire, and returns the backoff it waited.
    fn fail_and_retry(u: &mut Updater, v: &str) -> Duration {
        let next = u.finished(false, ver(v));
        assert!(u.take_retry(retry(&next)));
        delay(&next)
    }

    #[test]
    fn a_new_version_installs_and_restarts() {
        let mut u = running("1.0.0");
        assert_eq!(u.on_change(None), None, "unreadable VERSION");
        assert_eq!(u.on_change(ver("1.0.0")), None, "already running it");
        assert_eq!(u.on_change(ver("2.0.0")), install("2.0.0"));
        assert_eq!(u.on_change(ver("2.0.0")), None, "one install at a time");
        assert_eq!(u.finished(true, ver("2.0.0")), restart("2.0.0"));
    }

    #[test]
    fn a_daemon_behind_its_checkout_installs_it() {
        // A daemon started from a stale bin/simul: the checkout already names a newer release.
        assert_eq!(running("1.0.0").on_change(ver("2.0.0")), install("2.0.0"));
    }

    #[test]
    fn a_version_named_during_an_install_is_installed_before_restarting() {
        let mut u = running("1.0.0");
        assert_eq!(u.on_change(ver("2.0.0")), install("2.0.0"));
        assert_eq!(u.on_change(ver("3.0.0")), None, "busy");
        assert_eq!(
            u.finished(true, ver("3.0.0")),
            Next::Install("3.0.0".into())
        );
        assert_eq!(u.finished(true, ver("3.0.0")), restart("3.0.0"));
    }

    #[test]
    fn a_failed_follow_up_install_backs_off_and_keeps_the_installed_build() {
        let mut u = running("1.0.0");
        assert_eq!(u.on_change(ver("2.0.0")), install("2.0.0"));
        assert_eq!(
            u.finished(true, ver("3.0.0")),
            Next::Install("3.0.0".into())
        );
        assert_eq!(delay(&u.finished(false, ver("3.0.0"))), RETRY_FIRST);
        assert_eq!(
            u.on_change(ver("2.0.0")),
            Some(restart("2.0.0")),
            "back to the build already in bin/simul"
        );
    }

    #[test]
    fn a_failure_after_a_restart_waits_instead_of_restarting_again() {
        // The restarted daemon must not treat its own version as a build waiting to run, or
        // each failed install would restart it into a fresh attempt with no backoff.
        let mut u = running("2.0.0");
        assert!(!u.restarted_for("2.0.0"));
        assert_eq!(u.on_change(ver("3.0.0")), install("3.0.0"));
        assert_eq!(delay(&u.finished(false, ver("3.0.0"))), RETRY_FIRST);
    }

    #[test]
    fn a_failed_install_is_retried_with_backoff() {
        let mut u = running("1.0.0");
        assert_eq!(u.on_change(ver("2.0.0")), install("2.0.0"));
        assert_eq!(fail_and_retry(&mut u, "2.0.0"), RETRY_FIRST);
        assert_eq!(
            u.on_change(ver("2.0.0")),
            install("2.0.0"),
            "the retry installs"
        );
        assert_eq!(fail_and_retry(&mut u, "2.0.0"), RETRY_FIRST * 2);
        assert_eq!(u.on_change(ver("2.0.0")), install("2.0.0"));
        assert_eq!(u.finished(true, ver("2.0.0")), restart("2.0.0"));
    }

    #[test]
    fn a_repeated_version_event_waits_for_the_armed_retry() {
        let mut u = running("1.0.0");
        u.on_change(ver("2.0.0"));
        let next = u.finished(false, ver("2.0.0"));
        assert_eq!(u.on_change(ver("2.0.0")), None, "the retry will try it");
        assert!(u.take_retry(retry(&next)));
        assert_eq!(u.on_change(ver("2.0.0")), install("2.0.0"));
    }

    #[test]
    fn a_newly_named_version_starts_its_backoff_afresh() {
        let mut u = running("1.0.0");
        for _ in 0..3 {
            assert_eq!(u.on_change(ver("2.0.0")), install("2.0.0"));
            fail_and_retry(&mut u, "2.0.0");
        }
        assert_eq!(u.on_change(ver("2.0.1")), install("2.0.1"));
        assert_eq!(delay(&u.finished(false, ver("2.0.1"))), RETRY_FIRST);
    }

    #[test]
    fn only_the_latest_retry_is_due() {
        let mut u = running("1.0.0");
        u.on_change(ver("2.0.0"));
        let first = retry(&u.finished(false, ver("2.0.0")));
        assert_eq!(
            u.on_change(ver("2.1.0")),
            install("2.1.0"),
            "a newly named version doesn't wait for the old retry"
        );
        assert!(!u.take_retry(first), "an install is running");
        let second = retry(&u.finished(false, ver("2.1.0")));
        assert!(!u.take_retry(first), "superseded");
        assert!(u.take_retry(second));
    }

    #[test]
    fn a_failed_restart_backs_off_further_each_time() {
        let mut u = running("1.0.0");
        let mut waits = Vec::new();
        for _ in 0..3 {
            assert_eq!(u.on_change(ver("2.0.0")), install("2.0.0"));
            assert_eq!(u.finished(true, ver("2.0.0")), restart("2.0.0"));
            let next = u.restart_failed();
            waits.push(delay(&next));
            assert!(u.take_retry(retry(&next)));
        }
        assert_eq!(waits, [RETRY_FIRST, RETRY_FIRST * 2, RETRY_FIRST * 4]);
    }

    #[test]
    fn retries_back_off_to_an_hour() {
        assert_eq!(retry_delay(1), RETRY_FIRST);
        assert_eq!(retry_delay(3), RETRY_FIRST * 4);
        assert_eq!(retry_delay(7), RETRY_MAX);
        assert_eq!(retry_delay(u32::MAX), RETRY_MAX);
    }

    #[test]
    fn going_back_to_the_running_version_installs_nothing() {
        let mut u = running("1.0.0");
        assert_eq!(u.on_change(ver("1.0.0")), None);
        assert_eq!(
            u.on_change(ver("2.0.0")),
            install("2.0.0"),
            "nothing was recorded"
        );
    }

    #[test]
    fn a_restart_that_runs_another_version_does_not_install_again() {
        // The release named 2.0.0 reports 1.0.0, so each restart would install it again.
        let mut u = running("1.0.0");
        assert!(u.restarted_for("2.0.0"), "mismatch is reported");
        assert_eq!(u.on_change(ver("2.0.0")), None);
        assert_eq!(
            u.on_change(ver("3.0.0")),
            install("3.0.0"),
            "a newer one still installs"
        );
    }

    #[test]
    fn a_restart_into_the_installed_version_is_quiet() {
        let mut u = running("2.0.0");
        assert!(!u.restarted_for("2.0.0"));
        assert_eq!(u.on_change(ver("2.0.0")), None);
    }

    #[test]
    fn restart_args_carry_only_the_latest_version() {
        let args = |a: &[&str]| a.iter().map(OsString::from).collect::<Vec<_>>();
        assert_eq!(
            restart_args(&args(&["daemon", "--replace"]), "2.0.0"),
            args(&["daemon", "--replace", "--restarted-for", "2.0.0"])
        );
        assert_eq!(
            restart_args(
                &args(&["daemon", "--restarted-for", "1.0.0", "--tmux-socket", "/t"]),
                "2.0.0"
            ),
            args(&["daemon", "--tmux-socket", "/t", "--restarted-for", "2.0.0"])
        );
        assert_eq!(
            restart_args(&args(&["daemon", "--restarted-for=1.0.0"]), "2.0.0"),
            args(&["daemon", "--restarted-for", "2.0.0"])
        );
    }

    fn plugin_at(root: &Path) -> PathBuf {
        let exe = root.join("bin/simul");
        std::fs::create_dir_all(root.join("bin")).unwrap();
        std::fs::create_dir_all(root.join("scripts")).unwrap();
        std::fs::write(&exe, "").unwrap();
        std::fs::write(root.join("VERSION"), " 1.2.3\n").unwrap();
        std::fs::write(root.join("scripts/install-binary.sh"), "").unwrap();
        exe
    }

    #[test]
    fn a_plugin_binary_finds_its_checkout() {
        let dir = tempfile::tempdir().unwrap();
        let exe = plugin_at(dir.path());
        assert_eq!(plugin_dir(&exe).as_deref(), Some(dir.path()));
        assert_eq!(wanted_version(dir.path()).as_deref(), Some("1.2.3"));
    }

    #[test]
    fn other_binaries_have_no_checkout() {
        let dir = tempfile::tempdir().unwrap();
        let exe = plugin_at(dir.path());
        assert_eq!(plugin_dir(&dir.path().join("target/release/simul")), None);
        std::fs::remove_file(dir.path().join("scripts/install-binary.sh")).unwrap();
        assert_eq!(plugin_dir(&exe), None, "no installer");
        std::fs::write(dir.path().join("VERSION"), "\n").unwrap();
        assert_eq!(wanted_version(dir.path()), None, "empty VERSION");
    }

    #[test]
    fn replace_without_a_daemon_does_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let me = Identity::of(Path::new("/x/simul"));
        replace(&dir.path().join("none.sock"), &me).unwrap();
    }
}
