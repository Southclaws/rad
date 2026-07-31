use std::fs;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};

use serde::Serialize;

use super::generated::InitArgs;
use super::project::{Configuration, DEFAULT_CONFIG_FILE, DEFAULT_SCHEMA_FILE, GenerationTarget};
use crate::process::Result;

const EMPTY_SCHEMA: &str =
    "# yaml-language-server: $schema=https://www.radengine.dev/rad.schema.json\ntables: []\n";
const STARTER_SCHEMA: &str = r#"# yaml-language-server: $schema=https://www.radengine.dev/rad.schema.json
tables:
  - id: 1
    name: users
    columns:
      - { id: 1, name: id, type: string, pk: true, default: uuid() }
      - { id: 2, name: handle, type: string, unique: true }
      - { id: 3, name: joined_at, type: int64, format: unix_ms, default: now_ms() }
"#;

pub(super) fn run(mut args: InitArgs, non_interactive: bool) -> Result<InitializedProject> {
    let destination = Destination::new(&args.directory)?;
    destination.check_available()?;

    let interactive = !args.yes;
    if interactive {
        if non_interactive {
            return Err(
                "rad init requires --yes when --non-interactive is set; pass every non-default choice explicitly"
                    .into(),
            );
        }
        if !io::stdin().is_terminal() || !io::stderr().is_terminal() {
            return Err(
                "rad init needs a terminal for guided setup\n\nRun rad init --yes to accept defaults, or pass the desired options alongside --yes."
                    .into(),
            );
        }
        prompt(&destination, &mut args)?;
    }

    let settings = Settings::from_args(args)?;
    let created = destination.create(&settings)?;
    let summary = format!(
        "Created:\n  {}\n  {}\n\nNext:\n  rad serve --catalog-mode schema\n\nThen, from {} in another terminal:\n  rad schema migrate",
        created.config_file.display(),
        created.schema_file.display(),
        destination.root.display()
    );
    if interactive {
        cliclack::outro_note("Rad project initialized", summary)?;
    }
    Ok(InitializedProject {
        root: destination.root,
        config_file: created.config_file,
        schema_file: created.schema_file,
        interactive,
    })
}

#[derive(Serialize)]
pub(super) struct InitializedProject {
    pub root: PathBuf,
    pub config_file: PathBuf,
    pub schema_file: PathBuf,
    #[serde(skip)]
    pub interactive: bool,
}

fn prompt(destination: &Destination, args: &mut InitArgs) -> Result {
    cliclack::intro(format!("Initialize {}", destination.root.display()))?;

    args.database_url = cliclack::input("Database URL")
        .default_input(&args.database_url)
        .validate(|value: &String| {
            super::client::Client::connect(value)
                .map(|_| ())
                .map_err(|error| error.to_string())
        })
        .interact()?;

    args.no_generate = !cliclack::confirm("Configure a generated Go client?")
        .initial_value(!args.no_generate)
        .interact()?;
    if !args.no_generate {
        let output: String = cliclack::input("Generated client directory")
            .default_input(&args.out.display().to_string())
            .validate(non_empty("Output directory"))
            .interact()?;
        args.out = output.into();
        args.pkg = cliclack::input("Go package name")
            .default_input(&args.pkg)
            .validate(non_empty("Package name"))
            .interact()?;
    }

    args.empty = !cliclack::confirm("Include a starter users table?")
        .initial_value(!args.empty)
        .interact()?;
    Ok(())
}

fn non_empty(label: &'static str) -> impl Fn(&String) -> std::result::Result<(), String> {
    move |value| {
        if value.trim().is_empty() {
            Err(format!("{label} is required"))
        } else {
            Ok(())
        }
    }
}

struct Settings {
    config: Configuration,
    schema: &'static str,
}

impl Settings {
    fn from_args(args: InitArgs) -> Result<Self> {
        super::client::Client::connect(&args.database_url)
            .map_err(|error| format!("invalid --database-url: {error}"))?;
        if !args.no_generate && args.out.as_os_str().is_empty() {
            return Err("--out cannot be empty".into());
        }
        if !args.no_generate && args.pkg.trim().is_empty() {
            return Err("--pkg cannot be empty".into());
        }
        let generate = (!args.no_generate)
            .then(|| GenerationTarget {
                language: "go".into(),
                output: args.out,
                package: args.pkg,
            })
            .into_iter()
            .collect();
        Ok(Self {
            config: Configuration {
                database_url: args.database_url,
                generate,
            },
            schema: if args.empty {
                EMPTY_SCHEMA
            } else {
                STARTER_SCHEMA
            },
        })
    }
}

struct Destination {
    root: PathBuf,
    config_file: PathBuf,
    schema_file: PathBuf,
}

impl Destination {
    fn new(directory: &Path) -> Result<Self> {
        let root = std::path::absolute(directory)?;
        Ok(Self {
            config_file: root.join(DEFAULT_CONFIG_FILE),
            schema_file: root.join(DEFAULT_SCHEMA_FILE),
            root,
        })
    }

    fn check_available(&self) -> Result {
        match fs::metadata(&self.root) {
            Ok(metadata) if !metadata.is_dir() => {
                return Err(format!(
                    "cannot initialize {}: the destination is not a directory",
                    self.root.display()
                )
                .into());
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(format!(
                    "could not inspect project directory at {}: {error}",
                    self.root.display()
                )
                .into());
            }
        }
        for path in [&self.config_file, &self.schema_file] {
            match fs::symlink_metadata(path) {
                Ok(_) => {
                    return Err(format!(
                        "refusing to overwrite existing project file at {}",
                        path.display()
                    )
                    .into());
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(format!("could not inspect {}: {error}", path.display()).into());
                }
            }
        }
        Ok(())
    }

    fn create(&self, settings: &Settings) -> Result<CreatedProject> {
        fs::create_dir_all(&self.root).map_err(|error| {
            format!(
                "could not create project directory at {}: {error}",
                self.root.display()
            )
        })?;
        self.check_available()?;

        let config = serde_yaml::to_string(&settings.config)?;
        crate::engine::catalog::schema::parse(
            &self.schema_file.display().to_string(),
            settings.schema.as_bytes(),
        )?;
        create_file(&self.config_file, config.as_bytes())?;
        create_file(&self.schema_file, settings.schema.as_bytes())?;

        Ok(CreatedProject {
            config_file: self.config_file.clone(),
            schema_file: self.schema_file.clone(),
        })
    }
}

#[derive(Debug)]
struct CreatedProject {
    config_file: PathBuf,
    schema_file: PathBuf,
}

fn create_file(path: &Path, contents: &[u8]) -> Result {
    let parent = path
        .parent()
        .ok_or_else(|| format!("{} has no parent directory", path.display()))?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)
        .map_err(|error| format!("could not prepare {}: {error}", path.display()))?;
    temporary
        .write_all(contents)
        .map_err(|error| format!("could not write {}: {error}", path.display()))?;
    temporary
        .as_file()
        .sync_all()
        .map_err(|error| format!("could not sync {}: {error}", path.display()))?;
    temporary
        .persist_noclobber(path)
        .map_err(|error| format!("could not create {}: {}", path.display(), error.error))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(generate: bool, empty: bool) -> Settings {
        Settings {
            config: Configuration {
                database_url: "rad://127.0.0.1:7237".into(),
                generate: generate
                    .then(GenerationTarget::default)
                    .into_iter()
                    .collect(),
            },
            schema: if empty { EMPTY_SCHEMA } else { STARTER_SCHEMA },
        }
    }

    #[test]
    fn creates_a_complete_project_without_accepted_state() {
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("project");
        let destination = Destination::new(&root).unwrap();

        let created = destination.create(&settings(true, false)).unwrap();

        let project = super::super::project::Project::load(
            &created.config_file,
            Path::new(DEFAULT_SCHEMA_FILE),
        )
        .unwrap();
        assert_eq!(project.config.generate.len(), 1);
        let source = project.read_schema().unwrap();
        let schema = crate::engine::catalog::schema::parse("initialized", &source).unwrap();
        assert_eq!(schema.tables.len(), 1);
        assert!(!root.join(super::super::project::DEFAULT_STATE_DIR).exists());
    }

    #[test]
    fn supports_an_empty_schema_without_client_generation() {
        let directory = tempfile::tempdir().unwrap();
        let destination = Destination::new(directory.path()).unwrap();

        destination.create(&settings(false, true)).unwrap();

        let config = fs::read_to_string(directory.path().join(DEFAULT_CONFIG_FILE)).unwrap();
        assert!(!config.contains("generate:"), "{config}");
        assert_eq!(
            fs::read_to_string(directory.path().join(DEFAULT_SCHEMA_FILE)).unwrap(),
            EMPTY_SCHEMA
        );
    }

    #[test]
    fn refuses_existing_files_without_touching_the_other_file() {
        let directory = tempfile::tempdir().unwrap();
        let config_file = directory.path().join(DEFAULT_CONFIG_FILE);
        fs::write(&config_file, "keep me\n").unwrap();
        let destination = Destination::new(directory.path()).unwrap();

        let error = destination
            .create(&settings(true, false))
            .unwrap_err()
            .to_string();

        assert!(error.contains("refusing to overwrite"), "{error}");
        assert_eq!(fs::read_to_string(config_file).unwrap(), "keep me\n");
        assert!(!directory.path().join(DEFAULT_SCHEMA_FILE).exists());
    }

    #[test]
    fn rejects_a_non_directory_destination() {
        let directory = tempfile::tempdir().unwrap();
        let destination_path = directory.path().join("project");
        fs::write(&destination_path, "not a directory").unwrap();
        let destination = Destination::new(&destination_path).unwrap();

        let error = destination.check_available().unwrap_err().to_string();

        assert!(error.contains("not a directory"), "{error}");
    }
}
