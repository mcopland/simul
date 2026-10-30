use anyhow::{Context, Result};

use crate::protocol::GitStatus;

/// The repo state in `cwd`, or `None` outside a repo.
pub async fn status(cwd: &str) -> Result<Option<GitStatus>> {
    let out = tokio::process::Command::new("git")
        // Like starship: never take index.lock away from the user's own git commands.
        .args(["--no-optional-locks", "-C", cwd, "status"])
        .args(["--porcelain=v2", "--branch", "--show-stash"])
        .stderr(std::process::Stdio::null())
        .output()
        .await
        .with_context(|| format!("failed to run git status in {cwd}"))?;
    if !out.status.success() {
        return Ok(None);
    }
    Ok(Some(parse_status(&String::from_utf8_lossy(&out.stdout))))
}

/// Parses `git status --porcelain=v2 --branch --show-stash` output.
pub fn parse_status(out: &str) -> GitStatus {
    let mut s = GitStatus::default();
    for line in out.lines() {
        let mut words = line.split(' ');
        match words.next() {
            Some("#") => match (words.next(), words.next()) {
                (Some("branch.head"), Some(head)) if head != "(detached)" => {
                    s.branch = Some(line["# branch.head ".len()..].to_owned());
                }
                (Some("branch.ab"), Some(ahead)) => {
                    s.ahead = count(ahead.trim_start_matches('+'));
                    s.behind = count(words.next().unwrap_or("").trim_start_matches('-'));
                }
                (Some("stash"), Some(n)) => s.stashed = count(n),
                _ => {}
            },
            Some("1" | "2") => {
                let xy = words.next().unwrap_or("..").as_bytes();
                let (x, y) = (xy.first().copied(), xy.get(1).copied());
                if x.is_some_and(|x| x != b'.') {
                    s.staged += 1;
                }
                if matches!(y, Some(b'M' | b'T')) {
                    s.modified += 1;
                }
                if x == Some(b'D') || y == Some(b'D') {
                    s.deleted += 1;
                }
            }
            Some("u") => s.conflicted += 1,
            Some("?") => s.untracked += 1,
            _ => {}
        }
    }
    s
}

fn count(s: &str) -> u32 {
    s.parse().unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_clean_branch() {
        let s = parse_status(
            "# branch.oid abc\n# branch.head main\n# branch.upstream origin/main\n# branch.ab +0 -0\n",
        );
        assert_eq!(
            s,
            GitStatus {
                branch: Some("main".into()),
                ..GitStatus::default()
            }
        );
    }

    #[test]
    fn parses_a_detached_head_and_no_upstream() {
        let s = parse_status("# branch.oid abc\n# branch.head (detached)\n");
        assert_eq!(s, GitStatus::default());
    }

    #[test]
    fn counts_each_kind_of_entry() {
        let out = "\
# branch.oid abc
# branch.head feat/x y
# branch.ab +2 -3
# stash 4
1 M. N... 100644 100644 100644 a b staged.rs
1 .M N... 100644 100644 100644 a b modified.rs
1 MM N... 100644 100644 100644 a b both.rs
1 .T N... 100644 100644 100644 a b typechange
1 D. N... 100644 000000 000000 a b gone-staged
1 .D N... 100644 100644 000000 a b gone
2 R. N... 100644 100644 100644 a b R100 new\told
u UU N... 100644 100644 100644 100644 a b c conflict.rs
? new.txt
? other.txt
! ignored.txt
";
        assert_eq!(
            parse_status(out),
            GitStatus {
                branch: Some("feat/x y".into()),
                ahead: 2,
                behind: 3,
                conflicted: 1,
                staged: 4,
                deleted: 2,
                modified: 3,
                untracked: 2,
                stashed: 4,
            }
        );
    }

    fn git(dir: &std::path::Path, args: &[&str]) {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args([
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "-c",
                "commit.gpgsign=false",
            ])
            // The developer's own hooks may reject test commits.
            .args(["-c", "core.hooksPath=/dev/null"])
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {out:?}");
    }

    #[tokio::test]
    async fn reads_a_real_repo() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_str().unwrap();
        assert_eq!(status(path).await.unwrap(), None, "not a repo");
        git(dir.path(), &["init", "-q", "-b", "trunk"]);
        git(dir.path(), &["commit", "-q", "--allow-empty", "-m", "init"]);
        std::fs::write(dir.path().join("new.txt"), "x").unwrap();
        let s = status(path).await.unwrap().unwrap();
        assert_eq!(s.branch.as_deref(), Some("trunk"));
        assert_eq!(s.untracked, 1);
    }
}
