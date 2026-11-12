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
