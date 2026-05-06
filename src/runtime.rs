use std::{env, path::PathBuf};

#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct RuntimePaths {
    pub dir: PathBuf,
    pub socket: PathBuf,
}

impl RuntimePaths {
    pub fn default() -> Self {
        let dir = runtime_dir();
        let socket = dir.join("pz.sock");

        Self { dir, socket }
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
