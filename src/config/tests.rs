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
