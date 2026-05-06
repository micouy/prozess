use std::{
    collections::BTreeMap,
    env,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::protocol::EnvVar;

#[derive(Debug, Default)]
pub struct Config {
    pub run: RunConfig,
    pub env: Vec<EnvVar>,
}

#[derive(Debug, Default)]
pub struct RunConfig {
    pub inherit_env: bool,
    pub env_files: Vec<PathBuf>,
}

#[derive(Debug, Deserialize)]
struct FileConfig {
    #[serde(default)]
    run: FileRunConfig,

    #[serde(default)]
    env: BTreeMap<String, EnvValue>,
}

#[derive(Debug, Default, Deserialize)]
struct FileRunConfig {
    #[serde(default)]
    inherit_env: bool,

    #[serde(default)]
    env_files: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum EnvValue {
    String(String),
    List(Vec<String>),
}

impl Config {
    pub fn load() -> Result<Self> {
        load_from_path(config_path())
    }
}

fn load_from_path(path: PathBuf) -> Result<Config> {
    if !path.exists() {
        return Ok(Config::default());
    }

    let contents = std::fs::read_to_string(&path)
        .with_context(|| format!("failed to read config {}", path.display()))?;
    let file_config: FileConfig = toml::from_str(&contents)
        .with_context(|| format!("failed to parse config {}", path.display()))?;
    let config_dir = path.parent().unwrap_or(Path::new("."));
    let env_files = file_config
        .run
        .env_files
        .iter()
        .map(|path| config_path_value(path, config_dir))
        .collect::<Result<Vec<_>>>()?;
    let env = file_config
        .env
        .into_iter()
        .map(|(key, value)| {
            if key.is_empty() || key.contains('\0') {
                bail!("invalid config env key {key:?}");
            }

            Ok(EnvVar {
                key,
                value: value.into_string(),
            })
        })
        .collect::<Result<Vec<_>>>()?;

    Ok(Config {
        run: RunConfig {
            inherit_env: file_config.run.inherit_env,
            env_files,
        },
        env,
    })
}

fn config_path() -> PathBuf {
    if let Some(path) = env::var_os("PZ_CONFIG") {
        return PathBuf::from(path);
    }

    if let Some(path) = env::var_os("XDG_CONFIG_HOME") {
        return PathBuf::from(path).join("pz.toml");
    }

    home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".config")
        .join("pz.toml")
}

fn config_path_value(path: &str, config_dir: &Path) -> Result<PathBuf> {
    let path = expand_config_home(path);
    let path = if path.is_absolute() {
        path
    } else {
        config_dir.join(path)
    };

    Ok(path.components().collect())
}

fn expand_config_home(path: &str) -> PathBuf {
    // Rust's PathBuf intentionally treats "~/foo" as a literal relative path.
    // For config files only, support the common user-facing shorthand where a
    // leading "~/" means "$HOME/". CLI paths do not need this because shells
    // normally expand ~ before invoking pz.
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = home_dir() {
            return home.join(rest);
        }
    }

    PathBuf::from(path)
}

fn home_dir() -> Option<PathBuf> {
    env::var_os("HOME").map(PathBuf::from)
}

impl EnvValue {
    fn into_string(self) -> String {
        match self {
            Self::String(value) => value,
            Self::List(values) => values.join(":"),
        }
    }
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;

    #[test]
    fn missing_config_uses_defaults() -> Result<()> {
        let dir = tempdir()?;
        let config = load_from_path(dir.path().join("missing.toml"))?;

        assert!(!config.run.inherit_env);
        assert!(config.run.env_files.is_empty());
        assert!(config.env.is_empty());

        Ok(())
    }

    #[test]
    fn parses_run_and_env_config() -> Result<()> {
        let dir = tempdir()?;
        let config_path = dir.path().join("pz.toml");
        std::fs::write(
            &config_path,
            r#"
            [run]
            inherit_env = true
            env_files = ["env/common.env"]

            [env]
            PATH = ["/usr/local/bin", "/usr/bin", "/bin"]
            NODE_ENV = "development"
            "#,
        )?;

        let config = load_from_path(config_path)?;
        assert!(config.run.inherit_env);
        assert_eq!(
            config.run.env_files,
            vec![dir.path().join("env/common.env")]
        );
        assert!(
            config
                .env
                .iter()
                .any(|env| env.key == "PATH" && env.value == "/usr/local/bin:/usr/bin:/bin")
        );
        assert!(
            config
                .env
                .iter()
                .any(|env| env.key == "NODE_ENV" && env.value == "development")
        );

        Ok(())
    }
}
