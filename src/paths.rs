use std::path::{Path, PathBuf};

use anyhow::{Result, bail};

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
    if let Some(explicit) = env("SIMUL_SOCKET") {
        return Ok(PathBuf::from(explicit));
    }
    let Some(tmux_socket) = tmux_socket else {
        bail!("cannot derive the simul socket: no tmux server socket known");
    };
    let dir = match env("XDG_RUNTIME_DIR") {
        Some(run) => PathBuf::from(run).join("simul"),
        None => PathBuf::from(format!("/tmp/simul-{uid}")),
    };
    let hash = fnv64(tmux_socket.as_os_str().as_encoded_bytes());
    Ok(dir.join(format!("{hash:016x}.sock")))
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
    socket_path(env_var, uid(), tmux.as_deref())
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
