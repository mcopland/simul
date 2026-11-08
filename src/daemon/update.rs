//! Updating in place. A daemon running from a plugin checkout watches the checkout's VERSION
//! file; when an update (TPM's `git pull`) changes it, the daemon installs the matching release
//! and re-executes into it. Loading the plugin replaces a daemon of another build the same way.

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

/// What the daemon knows about updating itself from its plugin checkout.
#[derive(Debug)]
pub struct Updater {
    /// The plugin checkout this binary was installed into; `None` for other builds.
    pub plugin: Option<PathBuf>,
    /// The checkout's VERSION when last read, so repeated change events act once.
    version: Option<String>,
    /// An installer is running.
    installing: bool,
}

impl Updater {
    /// For the checkout at `plugin`, starting from the VERSION it names now.
    pub fn new(plugin: Option<PathBuf>) -> Updater {
        Updater {
            version: plugin.as_deref().and_then(wanted_version),
            plugin,
            installing: false,
        }
    }

    /// The version to install now that the checkout names `wanted`, if any: once per VERSION
    /// change, never while an install runs, and never the `running` one.
    pub fn on_change(&mut self, wanted: Option<String>, running: &str) -> Option<String> {
        if self.installing || wanted.is_none() || wanted == self.version {
            return None;
        }
        self.version.clone_from(&wanted);
        let version = wanted.filter(|v| v != running)?;
        self.installing = true;
        Some(version)
    }

    pub fn finished(&mut self) {
        self.installing = false;
    }
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

/// Replaces this process with `exe`, keeping its arguments. The daemon's socket is closed on
/// exec, so the new image's `bind` finds it stale and takes it over. Returns only on failure.
pub fn exec(exe: &Path) -> anyhow::Error {
    use std::os::unix::process::CommandExt;
    let err = std::process::Command::new(exe)
        .args(std::env::args_os().skip(1))
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
    /// Installs the release the plugin checkout now names, once per VERSION change.
    pub(super) fn on_plugin_changed(&mut self) {
        let Some(plugin) = self.updater.plugin.clone() else {
            return;
        };
        let wanted = wanted_version(&plugin);
        let Some(version) = self.updater.on_change(wanted, env!("CARGO_PKG_VERSION")) else {
            return;
        };
        log!("plugin updated to {version}; installing it");
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let result = install(&plugin).await.map_err(|e| format!("{e:#}"));
            // The daemon loop only stops at shutdown, when the install no longer matters.
            let _ = tx.send(Cmd::Installed { version, result }).await;
        });
    }

    /// Re-executes into the freshly installed binary, which takes over from tmux's state.
    pub(super) fn on_installed(&mut self, version: &str, result: Result<(), String>) {
        self.updater.finished();
        if let Err(e) = result {
            log!("failed to install {version}: {e}");
            return;
        }
        log!(
            "installed {version}; restarting as {}",
            self.opts.exe.display()
        );
        let e = exec(&self.opts.exe);
        log!("{e:#}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ver(v: &str) -> Option<String> {
        Some(v.to_owned())
    }

    /// An updater whose checkout last named `version`.
    fn at(version: &str) -> Updater {
        Updater {
            plugin: None,
            version: ver(version),
            installing: false,
        }
    }

    #[test]
    fn a_new_version_installs_once() {
        let mut u = at("1.0.0");
        assert_eq!(u.on_change(ver("1.0.0"), "1.0.0"), None, "unchanged");
        assert_eq!(u.on_change(None, "1.0.0"), None, "unreadable VERSION");
        assert_eq!(u.on_change(ver("2.0.0"), "1.0.0"), ver("2.0.0"));
        assert_eq!(
            u.on_change(ver("2.0.0"), "1.0.0"),
            None,
            "already installing"
        );
        assert_eq!(
            u.on_change(ver("3.0.0"), "1.0.0"),
            None,
            "busy until finished"
        );
        u.finished();
        assert_eq!(
            u.on_change(ver("2.0.0"), "1.0.0"),
            None,
            "2.0.0 was handled"
        );
        assert_eq!(
            u.on_change(ver("3.0.0"), "1.0.0"),
            ver("3.0.0"),
            "3.0.0 arrived while busy, so it was never recorded"
        );
    }

    #[test]
    fn going_back_to_the_running_version_installs_nothing() {
        let mut u = at("2.0.0");
        assert_eq!(u.on_change(ver("1.0.0"), "1.0.0"), None);
        assert_eq!(
            u.on_change(ver("2.0.0"), "1.0.0"),
            ver("2.0.0"),
            "1.0.0 was recorded and nothing is installing"
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
