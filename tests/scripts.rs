//! The release helper scripts in scripts/.

use std::path::Path;
use std::process::{Command, Output};

const ROOT: &str = env!("CARGO_MANIFEST_DIR");

fn changelog_section(version: &str, file: Option<&Path>) -> Output {
    let mut cmd = Command::new("bash");
    cmd.arg(Path::new(ROOT).join("scripts/changelog-section.sh"))
        .arg(version);
    if let Some(file) = file {
        cmd.arg(file);
    }
    cmd.output().unwrap()
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

const SAMPLE: &str = "# Changelog

## [Unreleased]

### Fixed

- Pending.

## [1.10.0] - 2026-01-02

### Added

- Ten.

## [1.1.0] - 2026-01-01

### Fixed

- One.

[Unreleased]: https://example.com/compare/v1.10.0...HEAD
[1.10.0]: https://example.com/compare/v1.1.0...v1.10.0
";

#[test]
fn prints_only_the_named_section() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("CHANGELOG.md");
    std::fs::write(&file, SAMPLE).unwrap();

    let out = changelog_section("1.10.0", Some(&file));
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(stdout(&out), "### Added\n\n- Ten.\n");

    let out = changelog_section("1.1.0", Some(&file));
    assert_eq!(
        stdout(&out),
        "### Fixed\n\n- One.\n",
        "stops before the link references"
    );

    let out = changelog_section("Unreleased", Some(&file));
    assert_eq!(stdout(&out), "### Fixed\n\n- Pending.\n");
}

#[test]
fn a_missing_section_fails_with_its_version() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("CHANGELOG.md");
    std::fs::write(&file, SAMPLE).unwrap();
    // "1.1" must not match the 1.1.0 or 1.10.0 headings.
    for version in ["1.1", "2.0.0"] {
        let out = changelog_section(version, Some(&file));
        assert!(!out.status.success(), "{version}");
        assert!(stdout(&out).is_empty(), "{version}");
        assert!(
            stderr(&out).contains(&format!("no section for {version}")),
            "{}",
            stderr(&out)
        );
    }
}

#[test]
fn the_changelog_has_a_section_for_version() {
    // The release publishes this section as its notes and fails without one.
    let version = std::fs::read_to_string(Path::new(ROOT).join("VERSION")).unwrap();
    let out = changelog_section(version.trim(), None);
    assert!(out.status.success(), "{}", stderr(&out));
}

/// A git checkout holding copies of this repo's version files, CHANGELOG.md, and the bump
/// script, with `unreleased` as the Unreleased notes.
fn release_checkout(unreleased: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let copy = |rel: &str| {
        let to = root.join(rel);
        std::fs::create_dir_all(to.parent().unwrap()).unwrap();
        std::fs::copy(Path::new(ROOT).join(rel), to).unwrap();
    };
    for rel in ["Cargo.toml", "Cargo.lock", "scripts/bump-version.sh"] {
        copy(rel);
    }
    std::fs::write(root.join("VERSION"), "1.2.3\n").unwrap();
    let manifest = std::fs::read_to_string(root.join("Cargo.toml")).unwrap();
    let current = std::fs::read_to_string(Path::new(ROOT).join("VERSION")).unwrap();
    let current = current.trim();
    std::fs::write(
        root.join("Cargo.toml"),
        manifest.replacen(
            &format!("version = \"{current}\""),
            "version = \"1.2.3\"",
            1,
        ),
    )
    .unwrap();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/lib.rs"), "").unwrap();
    std::fs::write(root.join("src/main.rs"), "fn main() {}\n").unwrap();
    std::fs::write(
        root.join("CHANGELOG.md"),
        format!(
            "# Changelog\n\n## [Unreleased]\n{unreleased}\n## [1.2.3] - 2026-01-01\n\n\
             ### Fixed\n\n- Old.\n\n\
             [Unreleased]: https://github.com/mcopland/simul/compare/v1.2.3...HEAD\n\
             [1.2.3]: https://github.com/mcopland/simul/compare/v1.2.2...v1.2.3\n"
        ),
    )
    .unwrap();
    let git = |args: &[&str]| {
        let out = Command::new("git")
            .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
            .args([
                "-c",
                "core.hooksPath=/dev/null",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .current_dir(root)
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {}", stderr(&out));
        stdout(&out)
    };
    git(&["init", "-q"]);
    // The lock must already match the manifest, as in a real checkout.
    let out = Command::new("cargo")
        .args(["update", "--workspace", "--offline", "--quiet"])
        .current_dir(root)
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    git(&["add", "-A"]);
    git(&["commit", "-qm", "chore: init"]);
    dir
}

fn bump(root: &Path, version: &str) -> Output {
    bump_with_path(root, version, &std::env::var("PATH").unwrap())
}

fn bump_with_path(root: &Path, version: &str, path: &str) -> Output {
    Command::new("bash")
        .arg(root.join("scripts/bump-version.sh"))
        .arg(version)
        .env("PATH", path)
        .env("GIT_CONFIG_COUNT", "4")
        .env("GIT_CONFIG_KEY_0", "user.name")
        .env("GIT_CONFIG_VALUE_0", "t")
        .env("GIT_CONFIG_KEY_1", "user.email")
        .env("GIT_CONFIG_VALUE_1", "t@example.com")
        .env("GIT_CONFIG_KEY_2", "core.hooksPath")
        .env("GIT_CONFIG_VALUE_2", "/dev/null")
        .env("GIT_CONFIG_KEY_3", "commit.gpgsign")
        .env("GIT_CONFIG_VALUE_3", "false")
        .output()
        .unwrap()
}

fn git_out(root: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .unwrap();
    stdout(&out)
}

#[test]
fn bump_updates_every_version_file_and_commits() {
    let dir = release_checkout("\n### Added\n\n- New.\n");
    let root = dir.path();
    let mode = |rel: &str| {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(root.join(rel))
            .unwrap()
            .permissions()
            .mode()
    };
    let manifest_mode = mode("Cargo.toml");

    let out = bump(root, "1.3.0");
    assert!(out.status.success(), "{}", stderr(&out));

    let read = |rel: &str| std::fs::read_to_string(root.join(rel)).unwrap();
    assert_eq!(read("VERSION"), "1.3.0\n");
    assert!(read("Cargo.toml").contains("\nversion = \"1.3.0\"\n"));
    assert_eq!(mode("Cargo.toml"), manifest_mode, "the file mode is kept");
    assert!(
        read("Cargo.lock").contains("name = \"simul\"\nversion = \"1.3.0\""),
        "{}",
        read("Cargo.lock")
    );
    let changelog = read("CHANGELOG.md");
    assert!(
        changelog.contains("## [Unreleased]\n\n## [1.3.0] - "),
        "{changelog}"
    );
    assert!(changelog.contains(
        "[Unreleased]: https://github.com/mcopland/simul/compare/v1.3.0...HEAD\n\
         [1.3.0]: https://github.com/mcopland/simul/compare/v1.2.3...v1.3.0\n"
    ));
    let section = changelog_section("1.3.0", Some(&root.join("CHANGELOG.md")));
    assert_eq!(stdout(&section), "### Added\n\n- New.\n");

    assert_eq!(
        git_out(root, &["log", "-1", "--format=%s"]),
        "chore(release): bump version to 1.3.0\n"
    );
    assert_eq!(
        git_out(root, &["status", "--porcelain"]),
        "",
        "all committed"
    );
    assert_eq!(git_out(root, &["tag"]), "", "no tag");
}

#[test]
fn bump_refuses_bad_input_without_changing_anything() {
    let dir = release_checkout("\n### Added\n\n- New.\n");
    let root = dir.path();
    let head = git_out(root, &["rev-parse", "HEAD"]);

    for (version, why) in [("1.3", "usage"), ("1.2.3", "already 1.2.3")] {
        let out = bump(root, version);
        assert!(!out.status.success(), "{version}");
        assert!(stderr(&out).contains(why), "{version}: {}", stderr(&out));
    }

    std::fs::write(root.join("src/lib.rs"), "// edit\n").unwrap();
    let out = bump(root, "1.3.0");
    assert!(stderr(&out).contains("uncommitted"), "{}", stderr(&out));

    let empty = release_checkout("\n");
    let out = bump(empty.path(), "1.3.0");
    assert!(stderr(&out).contains("no notes"), "{}", stderr(&out));
    assert_eq!(git_out(empty.path(), &["status", "--porcelain"]), "");

    assert_eq!(git_out(root, &["rev-parse", "HEAD"]), head);
    assert_eq!(
        std::fs::read_to_string(root.join("VERSION")).unwrap(),
        "1.2.3\n"
    );
}

#[test]
fn bump_restores_the_files_when_a_step_fails() {
    use std::os::unix::fs::PermissionsExt;
    let dir = release_checkout("\n### Added\n\n- New.\n");
    let root = dir.path();
    let fake = root.join("fake-bin");
    std::fs::create_dir(&fake).unwrap();
    std::fs::write(fake.join("cargo"), "#!/bin/sh\nexit 101\n").unwrap();
    std::fs::set_permissions(fake.join("cargo"), std::fs::Permissions::from_mode(0o755)).unwrap();
    // Untracked, so the clean-tree check must not see it.
    std::fs::write(root.join(".git/info/exclude"), "fake-bin/\n").unwrap();
    let path = format!("{}:{}", fake.display(), std::env::var("PATH").unwrap());

    let out = bump_with_path(root, "1.3.0", &path);

    assert!(!out.status.success());
    assert!(stderr(&out).contains("Cargo.lock"), "{}", stderr(&out));
    assert_eq!(git_out(root, &["status", "--porcelain"]), "", "restored");
}
