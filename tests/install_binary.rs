use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const SCRIPT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/scripts/install-binary.sh");
const ENTRY: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/simul.tmux");
const VERSION: &str = "0.1.0";

/// A throwaway plugin checkout (VERSION, simul.tmux, and the script) and a
/// fake release server directory served over file://.
struct Plugin {
    dir: tempfile::TempDir,
}

impl Plugin {
    fn new() -> Plugin {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("plugin/scripts")).unwrap();
        fs::copy(SCRIPT, dir.path().join("plugin/scripts/install-binary.sh")).unwrap();
        fs::copy(ENTRY, dir.path().join("plugin/simul.tmux")).unwrap();
        fs::write(dir.path().join("plugin/VERSION"), format!("{VERSION}\n")).unwrap();
        fs::create_dir_all(dir.path().join("stubs")).unwrap();
        write_script(
            &dir.path().join("stubs/tmux"),
            &format!(
                "#!/bin/sh\n\
                 echo \"$*\" >> '{}'\n\
                 case \"$1\" in\n\
                 -V) echo 'tmux 3.5a' ;;\n\
                 display-message) [ \"$2\" = -p ] && echo /fake/socket ;;\n\
                 esac\n\
                 exit 0\n",
                dir.path().join("tmux.log").display()
            ),
        );
        Plugin { dir }
    }

    fn root(&self) -> PathBuf {
        self.dir.path().join("plugin")
    }

    fn bin(&self) -> PathBuf {
        self.root().join("bin/simul")
    }

    fn releases(&self) -> PathBuf {
        self.dir.path().join("releases")
    }

    /// Publishes simul-<triple>.tar.gz whose binary reports `reports`, with a
    /// .sha256 sidecar holding `hash` (or the real hash when None).
    fn publish(&self, triple: &str, reports: &str, hash: Option<&str>) {
        let stage = self.dir.path().join(format!("stage-{triple}"));
        fs::create_dir_all(&stage).unwrap();
        write_stub(&stage.join("simul"), reports);
        fs::write(stage.join("LICENSE"), "MIT\n").unwrap();

        let dest = self.releases().join(format!("v{VERSION}"));
        fs::create_dir_all(&dest).unwrap();
        let archive = format!("simul-{triple}.tar.gz");
        let status = Command::new("tar")
            .arg("-czf")
            .arg(dest.join(&archive))
            .arg("-C")
            .arg(&stage)
            .args(["simul", "LICENSE"])
            .status()
            .unwrap();
        assert!(status.success(), "tar failed for {archive}");

        let real = sha256(&dest.join(&archive));
        let hash = hash.unwrap_or(&real);
        fs::write(
            dest.join(format!("{archive}.sha256")),
            format!("{hash}  {archive}\n"),
        )
        .unwrap();
    }

    fn run(&self, os: &str, arch: &str) -> Output {
        Command::new("bash")
            .arg(self.root().join("scripts/install-binary.sh"))
            .env("SIMUL_OS", os)
            .env("SIMUL_ARCH", arch)
            .env(
                "SIMUL_RELEASE_BASE_URL",
                format!("file://{}", self.releases().display()),
            )
            .output()
            .unwrap()
    }

    /// Loads simul.tmux the way TPM does, against a stub tmux that logs every
    /// command. PATH holds only the stubs and system dirs, so no real simul or
    /// tmux leaks in. Returns the tmux log.
    fn load_plugin(&self) -> String {
        let path = format!("{}:/usr/bin:/bin:/usr/sbin:/sbin", self.stubs().display());
        let out = Command::new("bash")
            .arg(self.root().join("simul.tmux"))
            .env("PATH", path)
            .env("SIMUL_OS", "Darwin")
            .env("SIMUL_ARCH", "arm64")
            .env(
                "SIMUL_RELEASE_BASE_URL",
                format!("file://{}", self.releases().display()),
            )
            .output()
            .unwrap();
        assert!(out.status.success(), "simul.tmux failed: {}", stderr(&out));
        fs::read_to_string(self.dir.path().join("tmux.log")).unwrap_or_default()
    }

    fn stubs(&self) -> PathBuf {
        self.dir.path().join("stubs")
    }

    fn installed_version(&self) -> String {
        let out = Command::new(self.bin()).arg("--version").output().unwrap();
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }
}

fn write_stub(path: &Path, reports: &str) {
    write_script(path, &format!("#!/bin/sh\necho 'simul {reports}'\n"));
}

fn write_script(path: &Path, body: &str) {
    fs::write(path, body).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

fn messages(log: &str) -> Vec<&str> {
    log.lines()
        .filter(|l| l.starts_with("display-message") && !l.starts_with("display-message -p"))
        .collect()
}

fn sha256(path: &Path) -> String {
    let out = Command::new("shasum")
        .args(["-a", "256"])
        .arg(path)
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .next()
        .unwrap()
        .to_string()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

#[test]
fn version_file_matches_cargo() {
    let version = fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/VERSION")).unwrap();
    assert_eq!(version.trim(), env!("CARGO_PKG_VERSION"));
}

#[test]
fn installs_matching_release() {
    let p = Plugin::new();
    p.publish("aarch64-apple-darwin", VERSION, None);

    let out = p.run("Darwin", "arm64");

    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert!(out.stdout.is_empty(), "stdout must stay quiet");
    let mode = fs::metadata(p.bin()).unwrap().permissions().mode();
    assert!(mode & 0o111 != 0, "bin/simul is not executable");
    assert_eq!(p.installed_version(), format!("simul {VERSION}"));
}

#[test]
fn maps_each_supported_platform() {
    for (os, arch, triple) in [
        ("Darwin", "arm64", "aarch64-apple-darwin"),
        ("Darwin", "x86_64", "x86_64-apple-darwin"),
        ("Linux", "aarch64", "aarch64-unknown-linux-musl"),
        ("Linux", "arm64", "aarch64-unknown-linux-musl"),
        ("Linux", "x86_64", "x86_64-unknown-linux-musl"),
    ] {
        let p = Plugin::new();
        p.publish(triple, VERSION, None);
        let out = p.run(os, arch);
        assert!(
            out.status.success(),
            "{os}/{arch} should fetch {triple}; stderr: {}",
            stderr(&out)
        );
    }
}

#[test]
fn rejects_checksum_mismatch() {
    let p = Plugin::new();
    p.publish("x86_64-unknown-linux-musl", VERSION, Some(&"0".repeat(64)));

    let out = p.run("Linux", "x86_64");

    assert!(!out.status.success());
    assert!(!p.bin().exists(), "a bad download must not be installed");
    let err = stderr(&out);
    assert!(
        err.contains("simul-x86_64-unknown-linux-musl.tar.gz"),
        "stderr: {err}"
    );
    assert!(
        err.contains(&"0".repeat(64)),
        "stderr should show the expected hash: {err}"
    );
}

#[test]
fn skips_download_when_current() {
    let p = Plugin::new();
    fs::create_dir_all(p.root().join("bin")).unwrap();
    write_stub(&p.bin(), VERSION);

    // Nothing is published, so any download attempt fails the run.
    let out = p.run("Darwin", "arm64");

    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(p.installed_version(), format!("simul {VERSION}"));
}

#[test]
fn replaces_stale_binary() {
    let p = Plugin::new();
    fs::create_dir_all(p.root().join("bin")).unwrap();
    write_stub(&p.bin(), "0.0.9");
    p.publish("aarch64-apple-darwin", VERSION, None);

    let out = p.run("Darwin", "arm64");

    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(p.installed_version(), format!("simul {VERSION}"));
}

#[test]
fn rejects_unsupported_platform() {
    let p = Plugin::new();

    let out = p.run("FreeBSD", "amd64");

    assert!(!out.status.success());
    assert!(!p.bin().exists());
    let err = stderr(&out);
    assert!(
        err.contains("FreeBSD") && err.contains("amd64"),
        "stderr: {err}"
    );
}

#[test]
fn reports_missing_release() {
    let p = Plugin::new();

    let out = p.run("Darwin", "arm64");

    assert!(!out.status.success());
    let err = stderr(&out);
    assert!(
        err.contains(&format!("v{VERSION}")),
        "stderr should name the version: {err}"
    );
}

#[test]
fn reports_unwritable_plugin_dir_with_context() {
    let p = Plugin::new();
    p.publish("aarch64-apple-darwin", VERSION, None);
    fs::create_dir_all(p.root().join("bin")).unwrap();
    fs::set_permissions(p.root().join("bin"), fs::Permissions::from_mode(0o555)).unwrap();

    let out = p.run("Darwin", "arm64");

    fs::set_permissions(p.root().join("bin"), fs::Permissions::from_mode(0o755)).unwrap();
    assert!(!out.status.success());
    let err = stderr(&out);
    let last = err.lines().last().unwrap_or_default();
    assert!(
        last.starts_with("simul install:") && last.contains("bin"),
        "last stderr line should say what failed: {err}"
    );
}

#[test]
fn plugin_installs_binary_and_binds_key() {
    let p = Plugin::new();
    p.publish("aarch64-apple-darwin", VERSION, None);

    let log = p.load_plugin();

    assert_eq!(p.installed_version(), format!("simul {VERSION}"));
    let bin = p.bin().display().to_string();
    assert!(
        log.lines()
            .any(|l| l.starts_with("bind-key a") && l.contains(&bin)),
        "{log}"
    );
    assert!(messages(&log).is_empty(), "unexpected messages: {log}");
}

#[test]
fn plugin_ignores_stderr_noise_from_a_successful_install() {
    let p = Plugin::new();
    p.publish("aarch64-apple-darwin", VERSION, None);
    write_script(
        &p.stubs().join("tar"),
        "#!/bin/sh\necho 'tar: ignoring unknown extended header' >&2\nexec /usr/bin/tar \"$@\"\n",
    );

    let log = p.load_plugin();

    assert_eq!(p.installed_version(), format!("simul {VERSION}"));
    assert!(messages(&log).is_empty(), "unexpected messages: {log}");
}

#[test]
fn plugin_reports_failed_update_and_keeps_old_binary() {
    let p = Plugin::new();
    fs::create_dir_all(p.root().join("bin")).unwrap();
    write_stub(&p.bin(), "0.0.9");

    let log = p.load_plugin();

    let msgs = messages(&log);
    assert_eq!(msgs.len(), 1, "{log}");
    assert!(
        msgs[0].contains("simul install: failed to download"),
        "{log}"
    );
    assert!(
        msgs[0].contains(&format!("using {}", p.bin().display())),
        "{log}"
    );
    assert!(log.lines().any(|l| l.starts_with("bind-key a")), "{log}");
}

#[test]
fn plugin_reports_missing_binary_with_install_error() {
    let p = Plugin::new();

    let log = p.load_plugin();

    let msgs = messages(&log);
    assert_eq!(msgs.len(), 1, "{log}");
    assert!(
        msgs[0].contains("binary not found") && msgs[0].contains("simul install:"),
        "{log}"
    );
    assert!(!log.lines().any(|l| l.starts_with("bind-key")), "{log}");
}

#[test]
fn plugin_uses_dev_build_without_downloading() {
    let p = Plugin::new();
    let dev = p.root().join("target/release/simul");
    fs::create_dir_all(dev.parent().unwrap()).unwrap();
    write_stub(&dev, "0.0.0-dev");

    let log = p.load_plugin();

    assert!(!p.bin().exists(), "a dev checkout must not download");
    assert!(messages(&log).is_empty(), "unexpected messages: {log}");
    let dev = dev.display().to_string();
    assert!(
        log.lines()
            .any(|l| l.starts_with("bind-key a") && l.contains(&dev)),
        "{log}"
    );
}
