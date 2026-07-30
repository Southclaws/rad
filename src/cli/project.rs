use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::process::Result;

#[cfg(test)]
pub const DEFAULT_CONFIG_FILE: &str = "rad.config.yaml";
pub const DEFAULT_SCHEMA_FILE: &str = "rad.schema.yaml";
pub const DEFAULT_STATE_DIR: &str = "rad.state";

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Configuration {
    pub database_url: String,
    #[serde(default)]
    pub generate: Vec<GenerationTarget>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GenerationTarget {
    #[serde(default = "default_language")]
    pub language: String,
    #[serde(default = "default_output")]
    pub output: PathBuf,
    #[serde(default = "default_package")]
    pub package: String,
}

#[derive(Clone, Debug)]
pub struct Project {
    pub root: PathBuf,
    pub schema_file: PathBuf,
    pub state_dir: PathBuf,
    pub config: Configuration,
}

impl Project {
    pub fn load(config_file: &Path, schema_file: &Path) -> Result<Self> {
        let config_file = absolute(config_file)?;
        let source = std::fs::read(&config_file)?;
        let config: Configuration = serde_yaml::from_slice(&source)
            .map_err(|error| format!("{}: {error}", config_file.display()))?;
        validate(&config_file, &config)?;
        let root = config_file
            .parent()
            .ok_or_else(|| format!("{} has no parent directory", config_file.display()))?
            .to_owned();
        let schema_file = if schema_file == Path::new(DEFAULT_SCHEMA_FILE) {
            root.join(DEFAULT_SCHEMA_FILE)
        } else {
            absolute(schema_file)?
        };
        Ok(Self {
            state_dir: root.join(DEFAULT_STATE_DIR),
            root,
            schema_file,
            config,
        })
    }

    pub fn read_schema(&self) -> Result<Vec<u8>> {
        std::fs::read(&self.schema_file).map_err(Into::into)
    }
}

fn validate(path: &Path, config: &Configuration) -> Result {
    if config.database_url.is_empty() {
        return Err(format!("{}: database_url is required", path.display()).into());
    }
    super::client::Client::connect(&config.database_url)
        .map_err(|error| format!("{}: database_url: {error}", path.display()))?;
    for (index, target) in config.generate.iter().enumerate() {
        if target.language != "go" {
            return Err(format!(
                "{}: generate[{index}].language {:?} is not supported",
                path.display(),
                target.language
            )
            .into());
        }
    }
    Ok(())
}

fn absolute(path: impl AsRef<Path>) -> Result<PathBuf> {
    let path = path.as_ref();
    if path.is_absolute() {
        return Ok(path.to_owned());
    }
    Ok(std::env::current_dir()?.join(path))
}

fn default_language() -> String {
    "go".into()
}

fn default_output() -> PathBuf {
    "generated".into()
}

fn default_package() -> String {
    "db".into()
}

impl Default for GenerationTarget {
    fn default() -> Self {
        Self {
            language: default_language(),
            output: default_output(),
            package: default_package(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_paths_are_rooted_at_the_configuration() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(
            directory.path().join(DEFAULT_CONFIG_FILE),
            "database_url: rad://localhost\n",
        )
        .unwrap();
        let project = Project::load(
            &directory.path().join(DEFAULT_CONFIG_FILE),
            Path::new(DEFAULT_SCHEMA_FILE),
        )
        .unwrap();
        assert_eq!(
            project.schema_file,
            directory.path().join(DEFAULT_SCHEMA_FILE)
        );
        assert!(project.config.generate.is_empty());
    }

    #[test]
    fn unknown_project_configuration_is_rejected() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(DEFAULT_CONFIG_FILE);
        std::fs::write(&path, "database_url: rad://localhost\nunknown: true\n").unwrap();
        assert!(Project::load(&path, Path::new(DEFAULT_SCHEMA_FILE)).is_err());
    }
}
