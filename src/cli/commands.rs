use std::io::{self, Write};
use std::path::Path;

use chrono::Utc;

use super::client::Client;
use super::generated::{
    GenerateArgs, GlobalArgs, Handler, InitArgs, SchemaDiffArgs, SchemaDiffFormat,
    SchemaMigrateArgs, SchemaOptions, SchemaPullArgs, SchemaStatusArgs, ServeArgs,
    ServeCatalogMode, ServeStorage, ValidateArgs,
};
use super::project::{Project, read_schema_file};
use super::state::{StateStore, matches_accepted};
use crate::engine::catalog::model::Mode;
use crate::http::generated::types::{SchemaDiffResult, SchemaMigrateResult, SchemaState};
use crate::process::{Config, Result, StorageConfig};

pub(super) struct App;

const ACCEPTED_STATE_RECOVERY: &str = "Apply the desired schema first with:\n  rad schema migrate\n\nIf the database already has the intended schema, recover it with:\n  rad schema pull";
const ACCEPTED_STATE_CONTEXT: &str = "schema.lock.json points to an immutable changelog snapshot of the exact schema version accepted by the database. rad generate reads this state so generated clients cannot target unapplied local changes; it does not create accepted state.";

impl Handler for App {
    type Error = Box<dyn std::error::Error + Send + Sync>;

    async fn init(&mut self, _globals: &GlobalArgs, args: InitArgs) -> Result {
        super::init::run(args)
    }

    async fn serve(&mut self, _globals: &GlobalArgs, args: ServeArgs) -> Result {
        let catalog_mode = args.catalog_mode.map(|mode| match mode {
            ServeCatalogMode::Direct => Mode::Direct,
            ServeCatalogMode::Schema => Mode::Schema,
        });
        let storage = match args.storage {
            ServeStorage::Memory => StorageConfig::Memory {
                path: args.storage_path,
            },
            ServeStorage::File => StorageConfig::File {
                directory: args.db,
                path: args.storage_path,
            },
            ServeStorage::S3 => StorageConfig::S3 {
                bucket: args
                    .s3_bucket
                    .ok_or("--s3-bucket or RAD_S3_BUCKET is required with --storage s3")?,
                path: args.s3_prefix.unwrap_or(args.storage_path),
                region: args.s3_region,
                endpoint: args.s3_endpoint,
            },
        };
        crate::process::serve(
            Config {
                address: crate::process::normalize_address(&args.addr),
                catalog_mode,
                storage,
            },
            crate::process::shutdown_signal(),
        )
        .await
    }

    async fn validate(&mut self, _globals: &GlobalArgs, args: ValidateArgs) -> Result {
        let source = read_schema_file(&args.file)?;
        let schema =
            crate::engine::catalog::schema::parse(&args.file.display().to_string(), &source)?;
        let columns = schema
            .tables
            .iter()
            .map(|table| table.def.columns.len())
            .sum::<usize>();
        println!(
            "valid  {} — {} tables, {} columns",
            args.file.display(),
            schema.tables.len(),
            columns
        );
        Ok(())
    }

    async fn schema_status(
        &mut self,
        _globals: &GlobalArgs,
        options: &SchemaOptions,
        _args: SchemaStatusArgs,
    ) -> Result {
        let (project, client) = open_project(options)?;
        let server = client.schema().await?;
        println!(
            "Server\n  version: {}\n  hash:    {}",
            server.schema_version, server.schema_hash
        );

        let store = StateStore::new(&project.state_dir);
        let accepted = store.load();
        match &accepted {
            Ok(accepted) => println!(
                "\nLocal accepted schema\n  version: {}\n  hash:    {}",
                accepted.lock.schema_version, accepted.lock.schema_hash
            ),
            Err(error) => println!(
                "\nLocal accepted schema\n  unavailable: {}",
                accepted_state_unavailable(&store, error.as_ref())
            ),
        }

        let changes = match project.read_schema() {
            Ok(source) => match client.schema_diff(String::from_utf8(source)?).await {
                Ok(diff) => {
                    println!("\nLocal schema\n  changes: {}", diff.changes.len());
                    Some(diff.changes.len())
                }
                Err(error) => {
                    println!("\nLocal schema\n  unavailable: {error}");
                    None
                }
            },
            Err(error) => {
                println!("\nLocal schema\n  unavailable: {error}");
                None
            }
        };

        let summary = status_summary(&server, accepted.as_ref().ok(), changes);
        println!("\nStatus: {summary}");
        Ok(())
    }

    async fn schema_diff(
        &mut self,
        _globals: &GlobalArgs,
        options: &SchemaOptions,
        args: SchemaDiffArgs,
    ) -> Result {
        let (project, client) = open_project(options)?;
        let source = String::from_utf8(project.read_schema()?)?;
        let diff = client.schema_diff(source).await?;
        match args.format {
            SchemaDiffFormat::Json => println!("{}", serde_json::to_string_pretty(&diff)?),
            SchemaDiffFormat::Text => print_diff(&diff),
        }
        Ok(())
    }

    async fn schema_migrate(
        &mut self,
        _globals: &GlobalArgs,
        options: &SchemaOptions,
        mut args: SchemaMigrateArgs,
    ) -> Result {
        let (project, client) = open_project(options)?;
        let source = String::from_utf8(project.read_schema()?)?;
        let diff = client.schema_diff(source.clone()).await?;
        if !diff.blocking.is_empty() {
            println!("Migration cannot be applied.");
            print_findings("Cannot apply", &diff.blocking);
            return Err("target schema constraints are not satisfied".into());
        }
        if !diff.destructive.is_empty() && !args.accept_data_loss {
            args.accept_data_loss = confirm_data_loss(&diff)?;
            if !args.accept_data_loss {
                return Err("migration cancelled".into());
            }
        }
        if !diff.changes.is_empty() {
            println!("Applying schema changes...");
        }
        let mut migration = client
            .schema_migrate(
                source,
                diff.current_version,
                diff.current_hash,
                args.accept_data_loss,
            )
            .await?;
        if migration.state.as_str() == "converging" {
            println!(
                "Waiting for {} online schema transition(s)...",
                migration.transition_ids.len()
            );
            migration = client.wait_for_migration(migration).await?;
        }
        let version = migration.schema_version;
        let store = StateStore::new(&project.state_dir);
        store.write_accepted(schema_state(migration))?;
        println!("Schema version {version} committed.");
        println!(
            "Snapshot written:\n  {}",
            store.snapshot_path(version as u64).display()
        );
        println!("Lockfile updated:\n  {}", store.lock_path().display());
        maybe_generate(&project, args.no_generate)
    }

    async fn schema_pull(
        &mut self,
        _globals: &GlobalArgs,
        options: &SchemaOptions,
        args: SchemaPullArgs,
    ) -> Result {
        let (project, client) = open_project(options)?;
        let server = client.schema().await?;
        let store = StateStore::new(&project.state_dir);
        let modified = local_schema_modified(&project.schema_file, &store, &server.schema_hash)?;
        if modified && !args.force {
            return Err(format!(
                "{} contains local schema changes; pull would overwrite it (use --force to back it up and replace it)",
                project.schema_file.display()
            )
            .into());
        }
        if modified {
            let backup = store.backup_desired(&project.schema_file, Utc::now())?;
            println!("Local schema backed up:\n  {}", backup.display());
        }
        let version = server.schema_version;
        let accepted = store.write_snapshot(server)?;
        StateStore::write_desired(&project.schema_file, &accepted.source)?;
        store.write_lock(&accepted.lock)?;
        println!("Pulled schema version {version}.");
        println!(
            "Updated:\n  {}\n  {}\n  {}",
            project.schema_file.display(),
            store.lock_path().display(),
            store.snapshot_path(version as u64).display()
        );
        maybe_generate(&project, args.no_generate)
    }

    async fn generate(&mut self, _globals: &GlobalArgs, args: GenerateArgs) -> Result {
        let target = super::project::GenerationTarget {
            language: match args.lang {
                super::generated::GenerateLang::Go => "go".into(),
            },
            output: args.out,
            package: args.pkg,
        };
        generate_target(&args.file, &target)
    }
}

fn open_project(options: &SchemaOptions) -> Result<(Project, Client)> {
    let project = Project::load(&options.config, &options.file)?;
    let client = Client::connect(&project.config.database_url)?;
    Ok((project, client))
}

fn print_diff(diff: &SchemaDiffResult) {
    if diff.changes.is_empty() {
        println!("No schema changes.");
    } else {
        println!("Schema changes");
        for change in &diff.changes {
            println!("  - {}", change.summary);
        }
    }
    print_findings("Data loss", &diff.destructive);
    print_findings("Cannot apply", &diff.blocking);
}

fn print_findings(title: &str, findings: &[crate::http::generated::types::SchemaFinding]) {
    if findings.is_empty() {
        return;
    }
    println!("\n{title}");
    for finding in findings {
        println!("  - {}", finding.summary);
    }
}

fn confirm_data_loss(diff: &SchemaDiffResult) -> Result<bool> {
    println!("WARNING: This migration will permanently delete data.");
    for finding in &diff.destructive {
        println!("\n- {}", finding.summary);
    }
    print!("\nContinue? [y/N] ");
    io::stdout().flush()?;
    let mut answer = String::new();
    io::stdin().read_line(&mut answer)?;
    Ok(matches!(
        answer.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

fn schema_state(migration: SchemaMigrateResult) -> SchemaState {
    SchemaState {
        schema: migration.schema,
        schema_hash: migration.schema_hash,
        schema_version: migration.schema_version,
    }
}

fn local_schema_modified(path: &Path, store: &StateStore, server_hash: &str) -> Result<bool> {
    let source = match std::fs::read(path) {
        Ok(source) => source,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => {
            return Err(
                format!("could not read schema file at {}: {error}", path.display()).into(),
            );
        }
    };
    match store.load() {
        Ok(accepted) => Ok(!matches_accepted(path, &source, &accepted)?),
        Err(_) => {
            let schema =
                crate::engine::catalog::schema::parse(&path.display().to_string(), &source)?;
            Ok(schema.canonical().hash()? != server_hash)
        }
    }
}

fn is_not_found(error: &(dyn std::error::Error + 'static)) -> bool {
    error
        .downcast_ref::<io::Error>()
        .is_some_and(|error| error.kind() == io::ErrorKind::NotFound)
}

fn accepted_state_unavailable(
    store: &StateStore,
    error: &(dyn std::error::Error + 'static),
) -> String {
    let problem = if is_not_found(error) {
        if store.lock_path().exists() {
            format!("accepted schema snapshot is missing: {error}")
        } else {
            format!(
                "no accepted schema state found at {}",
                store.lock_path().display()
            )
        }
    } else {
        format!("accepted schema state is invalid: {error}")
    };
    format!(
        "{problem}\n\n{ACCEPTED_STATE_CONTEXT}\n\nrad.state is managed by Rad; do not create or edit its lockfile or snapshots manually.\n\n{ACCEPTED_STATE_RECOVERY}"
    )
}

fn status_summary(
    server: &SchemaState,
    accepted: Option<&super::state::Accepted>,
    changes: Option<usize>,
) -> &'static str {
    let Some(accepted) = accepted else {
        return "local accepted schema state is unavailable";
    };
    let Ok(server_version) = u64::try_from(server.schema_version) else {
        return "server returned an invalid schema version";
    };
    if accepted.lock.schema_version == server_version
        && accepted.lock.schema_hash != server.schema_hash
    {
        return "schema history diverged";
    }
    if accepted.lock.schema_version < server_version {
        return "server is ahead of local accepted state";
    }
    if accepted.lock.schema_version > server_version {
        return "local accepted state is ahead of server";
    }
    match changes {
        None => "rad.schema.yaml is unavailable",
        Some(0) => "synchronized",
        Some(_) => "rad.schema.yaml contains unapplied changes",
    }
}

fn maybe_generate(project: &Project, disabled: bool) -> Result {
    if disabled {
        return Ok(());
    }
    for target in &project.config.generate {
        let output = if target.output.is_absolute() {
            target.output.clone()
        } else {
            project.root.join(&target.output)
        };
        let target = super::project::GenerationTarget {
            language: target.language.clone(),
            output,
            package: target.package.clone(),
        };
        generate_target(&project.schema_file, &target)?;
    }
    Ok(())
}

fn generate_target(schema_file: &Path, target: &super::project::GenerationTarget) -> Result {
    let schema_file = if schema_file.is_absolute() {
        schema_file.to_owned()
    } else {
        std::env::current_dir()?.join(schema_file)
    };
    let root = schema_file
        .parent()
        .ok_or_else(|| format!("{} has no parent directory", schema_file.display()))?;
    let desired = read_schema_file(&schema_file)?;
    let store = StateStore::new(root.join(super::project::DEFAULT_STATE_DIR));
    let lock_path = store.lock_path();
    if !lock_path.try_exists().map_err(|error| {
        format!(
            "could not inspect accepted schema state at {}: {error}",
            lock_path.display()
        )
    })? {
        return Err(format!(
            "no accepted schema state found at {}\n\n{ACCEPTED_STATE_CONTEXT}\n\nrad.state is managed by Rad; do not create or edit its lockfile or snapshots manually.\n\n{ACCEPTED_STATE_RECOVERY}",
            lock_path.display()
        )
        .into());
    }
    let accepted = store
        .load()
        .map_err(|error| accepted_state_unavailable(&store, error.as_ref()))?;
    if !matches_accepted(&schema_file, &desired, &accepted)? {
        return Err(format!(
            "{} differs from accepted schema version {}\n\nApply it with:\n  rad schema migrate\n\nOr restore the accepted schema with:\n  rad schema pull",
            schema_file.display(), accepted.lock.schema_version
        )
        .into());
    }

    let schema = crate::engine::catalog::schema::parse(
        &schema_file.display().to_string(),
        &accepted.source,
    )?
    .canonical();
    let files = crate::codegen::generate(
        &target.language,
        &schema,
        &crate::codegen::Options {
            package: target.package.clone(),
            schema_source: accepted.source,
            schema_version: accepted.lock.schema_version,
            schema_hash: accepted.lock.schema_hash,
        },
    )?;
    let paths = crate::codegen::publish(&target.output, &target.language, files)?;
    for path in paths {
        println!(
            "{}: generated {} ({} tables) for schema v{}",
            schema_file.display(),
            target.output.join(path).display(),
            schema.tables.len(),
            accepted.lock.schema_version
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_explains_how_to_create_missing_accepted_state() {
        let directory = tempfile::tempdir().unwrap();
        let store = StateStore::new(directory.path().join("rad.state"));
        let error = store.load().unwrap_err();

        let message = accepted_state_unavailable(&store, error.as_ref());

        assert!(
            message.contains(
                &directory
                    .path()
                    .join("rad.state")
                    .join("schema.lock.json")
                    .display()
                    .to_string()
            ),
            "{message}"
        );
        assert!(message.contains("rad schema migrate"), "{message}");
        assert!(message.contains("rad schema pull"), "{message}");
    }

    #[test]
    fn invalid_accepted_state_explains_ownership_and_recovery() {
        let directory = tempfile::tempdir().unwrap();
        let store = StateStore::new(directory.path());
        std::fs::write(
            store.lock_path(),
            r#"{"format_version":1,"schema_version":0,"schema_hash":"sha256:none","snapshot":""}"#,
        )
        .unwrap();
        let error = store.load().unwrap_err();

        let message = accepted_state_unavailable(&store, error.as_ref());

        assert!(
            message.contains("accepted schema state is invalid"),
            "{message}"
        );
        assert!(
            message.contains("immutable changelog snapshot"),
            "{message}"
        );
        assert!(message.contains("managed by Rad"), "{message}");
        assert!(message.contains("do not create or edit"), "{message}");
        assert!(message.contains("rad schema migrate"), "{message}");
        assert!(message.contains("rad schema pull"), "{message}");
    }

    #[test]
    fn missing_changelog_snapshot_explains_why_generate_needs_it() {
        let directory = tempfile::tempdir().unwrap();
        let store = StateStore::new(directory.path());
        std::fs::write(
            store.lock_path(),
            r#"{"format_version":1,"schema_version":0,"schema_hash":"sha256:none","snapshot":"changelog/00000000.rad.schema.yaml"}"#,
        )
        .unwrap();
        let error = store.load().unwrap_err();

        let message = accepted_state_unavailable(&store, error.as_ref());

        assert!(message.contains("snapshot is missing"), "{message}");
        assert!(
            message.contains("exact schema version accepted"),
            "{message}"
        );
        assert!(
            message.contains("does not create accepted state"),
            "{message}"
        );
        assert!(message.contains("rad schema migrate"), "{message}");
        assert!(message.contains("rad schema pull"), "{message}");
    }

    #[test]
    fn pull_recovery_still_detects_changes_when_local_state_is_invalid() {
        let directory = tempfile::tempdir().unwrap();
        let schema_file = directory.path().join("rad.schema.yaml");
        let source = b"tables:\n  - id: 1\n    name: users\n    columns:\n      - { id: 1, name: id, type: string, pk: true }\n";
        std::fs::write(&schema_file, source).unwrap();
        let canonical = crate::engine::catalog::schema::parse("fixture", source)
            .unwrap()
            .canonical();
        let store = StateStore::new(directory.path().join("rad.state"));
        std::fs::create_dir_all(directory.path().join("rad.state")).unwrap();
        std::fs::write(
            store.lock_path(),
            r#"{"format_version":1,"schema_version":0,"schema_hash":"sha256:none","snapshot":""}"#,
        )
        .unwrap();

        assert!(!local_schema_modified(&schema_file, &store, &canonical.hash().unwrap()).unwrap());
        std::fs::write(
            &schema_file,
            b"tables:\n  - id: 1\n    name: accounts\n    columns:\n      - { id: 1, name: id, type: string, pk: true }\n",
        )
        .unwrap();
        assert!(local_schema_modified(&schema_file, &store, &canonical.hash().unwrap()).unwrap());
    }

    #[test]
    fn generate_explains_how_to_create_missing_accepted_state() {
        let directory = tempfile::tempdir().unwrap();
        let schema_file = directory.path().join("rad.schema.yaml");
        std::fs::write(
            &schema_file,
            b"tables:\n  - id: 1\n    name: users\n    columns:\n      - { id: 1, name: id, type: string, pk: true }\n",
        )
        .unwrap();
        let target = crate::cli::project::GenerationTarget::default();

        let error = generate_target(&schema_file, &target)
            .unwrap_err()
            .to_string();

        assert!(
            error.contains(
                &directory
                    .path()
                    .join("rad.state")
                    .join("schema.lock.json")
                    .display()
                    .to_string()
            ),
            "{error}"
        );
        assert!(error.contains("immutable changelog snapshot"), "{error}");
        assert!(error.contains("does not create accepted state"), "{error}");
        assert!(error.contains("rad schema migrate"), "{error}");
        assert!(error.contains("rad schema pull"), "{error}");
    }
}
