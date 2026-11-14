use std::process::Command;

const BIN: &str = env!("CARGO_BIN_EXE_simul");

/// Outside a repo, a translated git must still read as "not a repo". This only fails where
/// git ships German messages; elsewhere it passes either way.
#[test]
fn project_kiro_hooks_outside_a_repo_under_a_translated_git() {
    let dir = tempfile::tempdir().unwrap();
    let out = Command::new(BIN)
        .args(["install-hooks", "kiro", "--project"])
        .current_dir(dir.path())
        .env("LANG", "de_DE.UTF-8")
        .env("LC_ALL", "de_DE.UTF-8")
        .env("LANGUAGE", "de")
        .env("GIT_CEILING_DIRECTORIES", dir.path().parent().unwrap())
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "exit {:?}\nstdout: {stdout}\nstderr: {}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(stdout.contains("not in a git repo"), "{stdout}");
}
