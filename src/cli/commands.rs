use std::io::{self, Write};
use std::path::Path;

use chrono::Utc;

use super::client::Client;
use super::generated::{
    GenerateArgs, GlobalArgs, Handler, SchemaDiffArgs, SchemaDiffFormat, SchemaMigrateArgs,
    SchemaOptions, SchemaPullArgs, SchemaStatusArgs, ServeArgs, ServeCatalogMode, ServeStorage,
    ValidateArgs,
};
use super::project::Project;
use super::state::{StateStore, matches_accepted};
use crate::engine::catalog::model::Mode;
use crate::http::generated::types::{SchemaDiffResult, SchemaMigrateResult, SchemaState};
use crate::process::{Config, Result, StorageConfig};

pub struct App;

impl Handler for App {
    type Error = Box<dyn std::error::Error + Send + Sync>;

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
        let source = std::fs::read(&args.file)?;
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
            Err(error) => println!("\nLocal accepted schema\n  unavailable: {error}"),
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
        store.write_desired(&project.schema_file, &accepted.source)?;
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
        Err(error) => return Err(error.into()),
    };
    match store.load() {
        Ok(accepted) => Ok(!matches_accepted(path, &source, &accepted)?),
        Err(error) if is_not_found(error.as_ref()) => {
            let schema =
                crate::engine::catalog::schema::parse(&path.display().to_string(), &source)?;
            Ok(schema.canonical().hash()? != server_hash)
        }
        Err(error) => Err(error),
    }
}

fn is_not_found(error: &(dyn std::error::Error + 'static)) -> bool {
    error
        .downcast_ref::<io::Error>()
        .is_some_and(|error| error.kind() == io::ErrorKind::NotFound)
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
    let store = StateStore::new(root.join(super::project::DEFAULT_STATE_DIR));
    let accepted = store
        .load()
        .map_err(|error| format!("load accepted schema state: {error}"))?;
    let desired = std::fs::read(&schema_file)?;
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
