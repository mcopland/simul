//! Updating in place. A daemon running from a plugin checkout watches the checkout's VERSION
//! file; when an update (TPM's `git pull`) changes it, the daemon installs the matching release
//! and re-executes into it. Loading the plugin replaces a daemon of another build the same way.

use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use tokio::sync::mpsc;

use super::Cmd;
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

#[cfg(test)]
mod tests {
    use super::*;

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
