use std::{env, path::PathBuf};

#[derive(Debug, Clone)]
pub struct RuntimePaths {
    pub socket: PathBuf,
    pub database: PathBuf,
}

impl RuntimePaths {
    pub fn default() -> Self {
        let runtime_dir = runtime_dir();
        let socket = runtime_dir.join("pz.sock");
        let state_dir = state_dir();
        let database = state_dir.join("pz.sqlite");

        Self { socket, database }
    }
}

fn runtime_dir() -> PathBuf {
    if let Some(path) = env::var_os("PZ_RUNTIME_DIR") {
        return PathBuf::from(path);
    }

    if let Some(path) = env::var_os("XDG_RUNTIME_DIR") {
        return PathBuf::from(path).join("pz");
    }

    fallback_runtime_dir()
}

/// Keyed by uid, not `$USER`: the uid always exists, cannot be spoofed by
/// the environment, and cannot collide between users in a shared tmpdir.
fn fallback_runtime_dir() -> PathBuf {
    let uid = nix::unistd::getuid();

    env::temp_dir().join(format!("pz-{uid}"))
}

fn state_dir() -> PathBuf {
    if let Some(path) = env::var_os("PZ_STATE_DIR") {
        return PathBuf::from(path);
    }

    if let Some(path) = env::var_os("XDG_STATE_HOME") {
        return PathBuf::from(path).join("pz");
    }

    if cfg!(target_os = "macos")
        && let Some(home) = home_dir()
    {
        return home.join("Library").join("Application Support").join("pz");
    }

    if let Some(home) = home_dir() {
        return home.join(".local").join("state").join("pz");
    }

    runtime_dir()
}

fn home_dir() -> Option<PathBuf> {
    env::var_os("HOME").map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fallback_runtime_dir_is_keyed_by_uid_not_user_env() {
        let dir = fallback_runtime_dir();
        let name = dir
            .file_name()
            .and_then(|name| name.to_str())
            .expect("fallback dir should have a utf-8 name");
        let uid = name
            .strip_prefix("pz-")
            .expect("fallback dir should start with pz-");

        assert!(!uid.is_empty(), "{name}");
        assert!(uid.bytes().all(|byte| byte.is_ascii_digit()), "{name}");
    }
}
