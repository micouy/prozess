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

    let user = env::var("USER")
        .or_else(|_| env::var("LOGNAME"))
        .unwrap_or_else(|_| "unknown".to_owned());

    env::temp_dir().join(format!("pz-{user}"))
}

fn state_dir() -> PathBuf {
    if let Some(path) = env::var_os("PZ_STATE_DIR") {
        return PathBuf::from(path);
    }

    if let Some(path) = env::var_os("XDG_STATE_HOME") {
        return PathBuf::from(path).join("pz");
    }

    if cfg!(target_os = "macos") {
        if let Some(home) = home_dir() {
            return home.join("Library").join("Application Support").join("pz");
        }
    }

    if let Some(home) = home_dir() {
        return home.join(".local").join("state").join("pz");
    }

    runtime_dir()
}

fn home_dir() -> Option<PathBuf> {
    env::var_os("HOME").map(PathBuf::from)
}
