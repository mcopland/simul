use std::path::{Path, PathBuf};

use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};

use anyhow::{Context, Result, bail};

/// FNV-1a, hand-rolled because std's hasher is not stable across Rust releases and the
/// shim and daemon may be built by different toolchains after an upgrade.
pub fn fnv64(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf29ce484222325, |h, b| {
        (h ^ u64::from(*b)).wrapping_mul(0x100000001b3)
    })
}

/// Resolves the daemon socket for a tmux server.
pub fn socket_path(
    env: impl Fn(&str) -> Option<String>,
    uid: u32,
    tmux_socket: Option<&Path>,
) -> Result<PathBuf> {
    let mut candidates = socket_candidates(env, uid, tmux_socket)?;
    Ok(candidates.remove(0))
}

/// Every place a daemon for this tmux server may listen, preferred first.
pub fn socket_candidates(
    env: impl Fn(&str) -> Option<String>,
    uid: u32,
    tmux_socket: Option<&Path>,
) -> Result<Vec<PathBuf>> {
    if let Some(explicit) = env("SIMUL_SOCKET") {
        return Ok(vec![PathBuf::from(explicit)]);
    }
    let Some(tmux_socket) = tmux_socket else {
        bail!("cannot derive the simul socket: no tmux server socket known");
    };
    let name = format!(
        "{:016x}.sock",
        fnv64(tmux_socket.as_os_str().as_encoded_bytes())
    );
    // The daemon (started under tmux's environment) and hooks (in a pane's shell) can
    // disagree about XDG_RUNTIME_DIR, so clients look in both places.
    let mut out: Vec<PathBuf> = env("XDG_RUNTIME_DIR")
        .map(|run| PathBuf::from(run).join("simul").join(&name))
        .into_iter()
        .collect();
    out.push(PathBuf::from(format!("/tmp/simul-{uid}")).join(&name));
    Ok(out)
}

/// Creates `dir` as 0700 if missing, then checks it is private to this user.
pub fn ensure_private_dir(dir: &Path) -> Result<()> {
    if !dir.exists() {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .with_context(|| format!("failed to create {}", dir.display()))?;
    }
    check_private_dir(dir)
}

/// Refuses directories another user owns or can write, since the socket inside carries
/// hook traffic and the log names working directories.
pub fn check_private_dir(dir: &Path) -> Result<()> {
    let meta = std::fs::symlink_metadata(dir)
        .with_context(|| format!("failed to stat {}", dir.display()))?;
    let mode = meta.permissions().mode() & 0o777;
    if !meta.is_dir() || meta.uid() != uid() || mode & 0o077 != 0 {
        bail!(
            "refusing {}: must be a directory owned by uid {} with mode 700 (found uid {}, mode {mode:o})",
            dir.display(),
            uid(),
            meta.uid()
        );
    }
    Ok(())
}

pub fn uid() -> u32 {
    // SAFETY: getuid has no preconditions and cannot fail.
    unsafe { libc::getuid() }
}

pub fn env_var(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.is_empty())
}

/// The tmux server socket named by `$TMUX` (`<socket>,<pid>,<session>`).
pub fn tmux_socket_from_env(value: &str) -> Option<PathBuf> {
    let socket = value.split(',').next()?;
    (!socket.is_empty()).then(|| PathBuf::from(socket))
}

pub fn current_tmux_socket() -> Option<PathBuf> {
    env_var("TMUX").and_then(|v| tmux_socket_from_env(&v))
}

/// Socket for commands run by the user, from `$SIMUL_SOCKET` or `$TMUX`.
pub fn client_socket() -> Result<PathBuf> {
    let tmux = current_tmux_socket();
    if env_var("SIMUL_SOCKET").is_none() && tmux.is_none() {
        bail!(
            "cannot locate the simul daemon: not inside tmux ($TMUX unset) and $SIMUL_SOCKET unset"
        );
    }
    let candidates = socket_candidates(env_var, uid(), tmux.as_deref())?;
    let chosen = candidates
        .iter()
        .find(|p| p.exists())
        .unwrap_or(&candidates[0])
        .clone();
    if let Some(dir) = chosen.parent().filter(|d| d.exists()) {
        check_private_dir(dir)?;
    }
    Ok(chosen)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |k| map.get(k).cloned()
    }

    #[test]
    fn fnv_matches_reference_vectors() {
        assert_eq!(fnv64(b""), 0xcbf29ce484222325);
        assert_eq!(fnv64(b"a"), 0xaf63dc4c8601ec8c);
        assert_eq!(fnv64(b"foobar"), 0x85944171f73967e8);
    }

    #[test]
    fn candidates_cover_both_locations() {
        let tmux = Path::new("/tmp/tmux-501/default");
        let name = format!("{:016x}.sock", fnv64(b"/tmp/tmux-501/default"));
        let c = socket_candidates(env(&[("XDG_RUNTIME_DIR", "/run/u")]), 501, Some(tmux)).unwrap();
        assert_eq!(
            c,
            vec![
                PathBuf::from("/run/u/simul").join(&name),
                PathBuf::from("/tmp/simul-501").join(&name)
            ]
        );
        let c = socket_candidates(env(&[("SIMUL_SOCKET", "/x.sock")]), 501, Some(tmux)).unwrap();
        assert_eq!(c, vec![PathBuf::from("/x.sock")]);
    }

    #[test]
    fn private_dir_is_created_and_checked() {
        use std::os::unix::fs::PermissionsExt;
        let base = tempfile::tempdir().unwrap();
        let dir = base.path().join("simul");
        ensure_private_dir(&dir).unwrap();
        let mode = std::fs::metadata(&dir).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700);
        check_private_dir(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        let err = check_private_dir(&dir).unwrap_err().to_string();
        assert!(err.contains("simul") && err.contains("755"), "{err}");
        assert!(ensure_private_dir(&dir).is_err());
    }

    #[test]
    fn explicit_socket_wins() {
        let p = socket_path(env(&[("SIMUL_SOCKET", "/x/s.sock")]), 501, None).unwrap();
        assert_eq!(p, PathBuf::from("/x/s.sock"));
    }

    #[test]
    fn runtime_dir_then_tmp() {
        let tmux = Path::new("/tmp/tmux-501/default");
        let name = format!("{:016x}.sock", fnv64(b"/tmp/tmux-501/default"));
        let p = socket_path(
            env(&[("XDG_RUNTIME_DIR", "/run/user/501")]),
            501,
            Some(tmux),
        )
        .unwrap();
        assert_eq!(p, PathBuf::from("/run/user/501/simul").join(&name));
        let p = socket_path(env(&[]), 501, Some(tmux)).unwrap();
        assert_eq!(p, PathBuf::from("/tmp/simul-501").join(&name));
    }

    #[test]
    fn needs_a_tmux_socket() {
        let err = socket_path(env(&[]), 501, None).unwrap_err();
        assert!(err.to_string().contains("tmux"), "{err}");
    }

    #[test]
    fn parses_tmux_env() {
        assert_eq!(
            tmux_socket_from_env("/tmp/tmux-501/default,73907,0"),
            Some(PathBuf::from("/tmp/tmux-501/default"))
        );
        assert_eq!(tmux_socket_from_env(""), None);
    }
}
