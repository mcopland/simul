mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use common::{BIN, Harness, eventually};

const SCRIPT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/scripts/install-binary.sh");
const VERSION: &str = env!("CARGO_PKG_VERSION");
/// The release the installer picks for the SIMUL_OS/SIMUL_ARCH these tests set.
const TRIPLE: &str = "aarch64-apple-darwin";

/// A copy of the test binary at `path`, which counts as another build. Returns its real path.
fn copy_bin(path: &Path) -> PathBuf {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::copy(BIN, path).unwrap();
    fs::canonicalize(path).unwrap()
}

/// A plugin checkout in the harness dir, as TPM leaves it: VERSION, the installer, and the
/// release binary in bin/simul.
fn plugin(h: &Harness) -> PathBuf {
    let root = h.dir.path().join("plugin");
    fs::create_dir_all(root.join("scripts")).unwrap();
    fs::copy(SCRIPT, root.join("scripts/install-binary.sh")).unwrap();
    fs::write(root.join("VERSION"), format!("{VERSION}\n")).unwrap();
    copy_bin(&root.join("bin/simul"));
    fs::canonicalize(root).unwrap()
}

/// Publishes the test binary as release `version` under `releases`, served over file://.
fn publish(releases: &Path, version: &str) {
    let stage = releases.join("stage");
    copy_bin(&stage.join("simul"));
    let dest = releases.join(format!("v{version}"));
    fs::create_dir_all(&dest).unwrap();
    let archive = format!("simul-{TRIPLE}.tar.gz");
    let status = Command::new("tar")
        .arg("-czf")
        .arg(dest.join(&archive))
        .arg("-C")
        .arg(&stage)
        .arg("simul")
        .status()
        .unwrap();
    assert!(status.success());
    let out = Command::new("shasum")
        .args(["-a", "256"])
        .arg(dest.join(&archive))
        .output()
        .unwrap();
    let hash = String::from_utf8_lossy(&out.stdout);
    let hash = hash.split_whitespace().next().unwrap();
    fs::write(
        dest.join(format!("{archive}.sha256")),
        format!("{hash}  {archive}\n"),
    )
    .unwrap();
}

/// The running daemon's binary, if it answers.
fn daemon_exe(h: &Harness) -> Option<String> {
    Some(h.identify()?["exe"].as_str()?.to_owned())
}

#[test]
fn replace_takes_over_from_another_build() {
    let mut h = Harness::new();
    h.start_daemon();
    let first = fs::canonicalize(BIN).unwrap();
    assert_eq!(daemon_exe(&h).as_deref(), first.to_str());
    assert_eq!(h.identify().unwrap()["version"], VERSION);

    let other = copy_bin(&h.dir.path().join("other/simul"));
    let cmd = h.simul_at(&other);
    h.start_daemon_with(cmd, &["--replace"]);
    eventually("the other build owns the socket", || {
        daemon_exe(&h).as_deref() == other.to_str()
    });
    let log = h.daemon_log();
    assert!(log.contains("replaced by another build"), "{log}");
}

#[test]
fn replace_leaves_the_same_build_running() {
    let mut h = Harness::new();
    h.start_daemon();
    let cmd = h.simul();
    h.start_daemon_with(cmd, &["--replace"]);
    eventually("the second daemon gives way", || {
        h.daemon_log().contains("daemon already running")
    });
    let log = h.daemon_log();
    assert!(!log.contains("replaced"), "{log}");
    assert_eq!(log.matches("daemon started").count(), 1, "{log}");
}

#[test]
fn a_new_plugin_version_installs_the_release_and_restarts() {
    let mut h = Harness::new();
    let root = plugin(&h);
    let releases = h.dir.path().join("releases");
    publish(&releases, "9.9.9");
    let mut cmd = h.simul_at(&root.join("bin/simul"));
    cmd.env("SIMUL_OS", "Darwin")
        .env("SIMUL_ARCH", "arm64")
        .env(
            "SIMUL_RELEASE_BASE_URL",
            format!("file://{}", releases.display()),
        );
    h.start_daemon_with(cmd, &[]);
    eventually("watching VERSION", || {
        h.daemon_log().contains("for updates")
    });

    // What TPM's `git pull` does to the checkout.
    fs::write(root.join("VERSION"), "9.9.9\n").unwrap();
    eventually("restarted into the new binary", || {
        h.daemon_log().matches("daemon started").count() == 2
    });
    let log = h.daemon_log();
    assert!(log.contains("installed 9.9.9"), "{log}");
    assert_eq!(
        daemon_exe(&h).as_deref(),
        root.join("bin/simul").to_str(),
        "{log}"
    );
    assert_eq!(
        log.matches("installing").count(),
        1,
        "the new daemon doesn't install again: {log}"
    );
}

#[test]
fn a_daemon_behind_its_checkout_installs_at_startup() {
    let mut h = Harness::new();
    let root = plugin(&h);
    let releases = h.dir.path().join("releases");
    publish(&releases, "9.9.9");
    // TPM updated the checkout while no daemon watched it; bin/simul is still the old build.
    fs::write(root.join("VERSION"), "9.9.9\n").unwrap();
    let mut cmd = h.simul_at(&root.join("bin/simul"));
    cmd.env("SIMUL_OS", "Darwin")
        .env("SIMUL_ARCH", "arm64")
        .env(
            "SIMUL_RELEASE_BASE_URL",
            format!("file://{}", releases.display()),
        );
    h.start_daemon_with(cmd, &[]);
    eventually("restarted into the new binary", || {
        h.daemon_log().matches("daemon started").count() == 2
    });
    // The test release reports this crate's version, not 9.9.9, so the restarted daemon must
    // see the mismatch rather than install again and restart forever.
    eventually("the restart notices the mismatch", || {
        h.daemon_log().contains("leaving it until VERSION changes")
    });
    let log = h.daemon_log();
    assert!(log.contains("installed 9.9.9"), "{log}");
    assert_eq!(log.matches("installing").count(), 1, "{log}");
    assert_eq!(log.matches("daemon started").count(), 2, "{log}");
}

#[test]
fn a_failed_install_keeps_the_daemon() {
    let mut h = Harness::new();
    let root = plugin(&h);
    let mut cmd = h.simul_at(&root.join("bin/simul"));
    cmd.env("SIMUL_OS", "Darwin")
        .env("SIMUL_ARCH", "arm64")
        .env(
            "SIMUL_RELEASE_BASE_URL",
            format!("file://{}", h.dir.path().join("no-releases").display()),
        );
    h.start_daemon_with(cmd, &[]);
    eventually("watching VERSION", || {
        h.daemon_log().contains("for updates")
    });
    fs::write(root.join("VERSION"), "9.9.9\n").unwrap();
    eventually("the install fails", || {
        h.daemon_log().contains("failed to install 9.9.9")
    });
    assert!(h.identify().is_some(), "still serving");
    let log = h.daemon_log();
    assert_eq!(log.matches("daemon started").count(), 1, "{log}");
}

/// (sidebar build tag, start command) of each sidebar pane.
fn sidebars(h: &Harness) -> Vec<(String, String)> {
    h.tmux(&[
        "list-panes",
        "-a",
        "-F",
        "#{@simul-sidebar}\t#{@simul-sidebar-exe}\t#{pane_start_command}",
    ])
    .lines()
    .filter_map(|l| {
        let mut f = l.splitn(3, '\t');
        (f.next() == Some("1")).then(|| {
            (
                f.next().unwrap_or_default().to_owned(),
                f.next().unwrap_or_default().to_owned(),
            )
        })
    })
    .collect()
}

#[test]
fn sidebars_restart_on_the_build_that_takes_over() {
    let mut h = Harness::new();
    h.start_daemon();
    let out = h.simul().arg("toggle").output().unwrap();
    assert!(out.status.success());
    // A tag is `<version> <mtime> <exe>`.
    let built_from = |tag: &str, exe: &Path| {
        tag.starts_with(&format!("{VERSION} ")) && tag.ends_with(&format!(" {}", exe.display()))
    };
    let first = fs::canonicalize(BIN).unwrap();
    eventually("a sidebar tagged with the first build", || {
        sidebars(&h).iter().any(|(t, _)| built_from(t, &first))
    });

    let other = copy_bin(&h.dir.path().join("other/simul"));
    let cmd = h.simul_at(&other);
    h.start_daemon_with(cmd, &["--replace"]);
    eventually("the sidebar runs the other build", || {
        let s = sidebars(&h);
        s.len() == 1 && built_from(&s[0].0, &other) && s[0].1.contains(other.to_str().unwrap())
    });
}
