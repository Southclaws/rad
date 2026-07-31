use std::io::{self, IsTerminal, Write};
use std::path::Path;
use std::time::{Duration, Instant};

use chrono::Utc;
use serde::Serialize;

use super::client::Client;
use super::generated::{
    DoctorArgs, GenerateArgs, GlobalArgs, Handler, InitArgs, SchemaDiffArgs, SchemaDiffFormat,
    SchemaJsonSchemaArgs, SchemaMigrateArgs, SchemaOptions, SchemaPullArgs, SchemaStatusArgs,
    SchemaTransitionsCancelArgs, SchemaTransitionsGetArgs, SchemaTransitionsListArgs,
    SchemaTransitionsListKind, SchemaTransitionsListState, SchemaTransitionsOptions,
    SchemaTransitionsWaitArgs, ServeArgs, ServeCatalogMode, ServeStorage, SkillsGetArgs,
    SkillsListArgs, SkillsOptions, SkillsPathArgs, SpecArgs, ValidateArgs,
};
use super::output::{self, CliError};
use super::project::{Project, read_schema_file};
use super::state::{StateStore, matches_accepted};
use crate::engine::catalog::model::Mode;
use crate::http::generated::types::{
    SchemaDiffResult, SchemaMigrateResult, SchemaState, TransitionControl, TransitionKind,
    TransitionState,
};
use crate::process::{Config, Result, StorageConfig};

pub(super) struct App;

const ACCEPTED_STATE_RECOVERY: &str = "Apply the desired schema first with:\n  rad schema migrate\n\nIf the database already has the intended schema, recover it with:\n  rad schema pull";
const ACCEPTED_STATE_CONTEXT: &str = "schema.lock.json points to an immutable changelog snapshot of the exact schema version accepted by the database. rad generate reads this state so generated clients cannot target unapplied local changes; it does not create accepted state.";

#[derive(Serialize)]
struct ValidateReport {
    valid: bool,
    file: std::path::PathBuf,
    tables: usize,
    columns: usize,
}

#[derive(Serialize)]
struct SchemaIdentity {
    schema_version: i64,
    schema_hash: String,
}

impl SchemaIdentity {
    fn from_server(state: &SchemaState) -> Self {
        Self {
            schema_version: state.schema_version,
            schema_hash: state.schema_hash.clone(),
        }
    }

    fn from_accepted(accepted: &super::state::Accepted) -> Self {
        Self {
            schema_version: i64::try_from(accepted.lock.schema_version).unwrap_or(i64::MAX),
            schema_hash: accepted.lock.schema_hash.clone(),
        }
    }
}

#[derive(Serialize)]
struct SchemaStatusReport {
    server: SchemaIdentity,
    #[serde(skip_serializing_if = "Option::is_none")]
    accepted: Option<SchemaIdentity>,
    #[serde(skip_serializing_if = "Option::is_none")]
    accepted_error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    desired_changes: Option<usize>,
    generated_clients: Vec<GeneratedClientStatus>,
    status: &'static str,
}

#[derive(Serialize)]
struct GeneratedClientStatus {
    language: String,
    path: std::path::PathBuf,
    state: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    schema_version: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    schema_hash: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Debug, Serialize)]
struct GeneratedArtifact {
    language: String,
    path: std::path::PathBuf,
    tables: usize,
    schema_version: u64,
}

#[derive(Serialize)]
struct SchemaMigrateReport {
    state: &'static str,
    schema_version: i64,
    schema_hash: String,
    changes: Vec<crate::http::generated::types::SchemaChange>,
    transition_ids: Vec<String>,
    snapshot: std::path::PathBuf,
    lockfile: std::path::PathBuf,
    generated: Vec<GeneratedArtifact>,
}

#[derive(Serialize)]
struct SchemaPullReport {
    schema_version: i64,
    schema_hash: String,
    desired_schema: std::path::PathBuf,
    lockfile: std::path::PathBuf,
    snapshot: std::path::PathBuf,
    #[serde(skip_serializing_if = "Option::is_none")]
    backup: Option<std::path::PathBuf>,
    generated: Vec<GeneratedArtifact>,
}

impl Handler for App {
    type Error = Box<dyn std::error::Error + Send + Sync>;

    async fn init(&mut self, globals: &GlobalArgs, args: InitArgs) -> Result {
        let initialized = super::init::run(args, globals.non_interactive)?;
        if output::is_json(globals) {
            output::print_json(&initialized)
        } else if !initialized.interactive {
            println!(
                "Rad project initialized.\n\nCreated:\n  {}\n  {}\n\nNext:\n  rad serve --catalog-mode schema\n\nThen, from {} in another terminal:\n  rad schema migrate",
                initialized.config_file.display(),
                initialized.schema_file.display(),
                initialized.root.display()
            );
            Ok(())
        } else {
            Ok(())
        }
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

    async fn validate(&mut self, globals: &GlobalArgs, args: ValidateArgs) -> Result {
        let source = read_schema_file(&args.file)?;
        let schema =
            crate::engine::catalog::schema::parse(&args.file.display().to_string(), &source)?;
        let columns = schema
            .tables
            .iter()
            .map(|table| table.def.columns.len())
            .sum::<usize>();
        let report = ValidateReport {
            valid: true,
            file: std::path::absolute(&args.file)?,
            tables: schema.tables.len(),
            columns,
        };
        if output::is_json(globals) {
            output::print_json(&report)
        } else {
            println!(
                "valid  {} — {} tables, {} columns",
                report.file.display(),
                report.tables,
                report.columns
            );
            Ok(())
        }
    }

    async fn doctor(&mut self, globals: &GlobalArgs, args: DoctorArgs) -> Result {
        super::doctor::run(globals, args).await
    }

    async fn schema_status(
        &mut self,
        globals: &GlobalArgs,
        options: &SchemaOptions,
        _args: SchemaStatusArgs,
    ) -> Result {
        let (project, client) = open_project(options)?;
        let server = client.schema().await?;
        let json_output = output::is_json(globals);
        if !json_output {
            println!(
                "Server\n  version: {}\n  hash:    {}",
                server.schema_version, server.schema_hash
            );
        }

        let store = StateStore::new(&project.state_dir);
        let accepted = store.load();
        if !json_output {
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
        }

        let changes = match project.read_schema() {
            Ok(source) => match client.schema_diff(String::from_utf8(source)?).await {
                Ok(diff) => {
                    if !json_output {
                        println!("\nLocal schema\n  changes: {}", diff.changes.len());
                    }
                    Some(diff.changes.len())
                }
                Err(error) => {
                    if !json_output {
                        println!("\nLocal schema\n  unavailable: {error}");
                    }
                    None
                }
            },
            Err(error) => {
                if !json_output {
                    println!("\nLocal schema\n  unavailable: {error}");
                }
                None
            }
        };

        let generated_clients = generated_client_statuses(&project, accepted.as_ref().ok());
        if !json_output && !generated_clients.is_empty() {
            println!("\nGenerated clients");
            for client in &generated_clients {
                println!(
                    "  {} ({})\n    state:   {}",
                    client.path.display(),
                    client.language,
                    client.state
                );
                if let Some(version) = client.schema_version {
                    println!("    version: {version}");
                }
                if let Some(hash) = &client.schema_hash {
                    println!("    hash:    {hash}");
                }
                if let Some(error) = &client.error {
                    println!("    error:   {error}");
                }
            }
        }
        let generated_match = generated_clients
            .iter()
            .all(|client| client.state == "synchronized");
        let summary = status_summary(&server, accepted.as_ref().ok(), changes, generated_match);
        if output::is_json(globals) {
            let report = SchemaStatusReport {
                server: SchemaIdentity::from_server(&server),
                accepted: accepted.as_ref().ok().map(SchemaIdentity::from_accepted),
                accepted_error: accepted
                    .as_ref()
                    .err()
                    .map(|error| accepted_state_unavailable(&store, error.as_ref())),
                desired_changes: changes,
                generated_clients,
                status: summary,
            };
            output::print_json(&report)
        } else {
            println!("\nStatus: {summary}");
            Ok(())
        }
    }

    async fn schema_diff(
        &mut self,
        globals: &GlobalArgs,
        options: &SchemaOptions,
        args: SchemaDiffArgs,
    ) -> Result {
        let (project, client) = open_project(options)?;
        let source = String::from_utf8(project.read_schema()?)?;
        let diff = client.schema_diff(source).await?;
        match (output::is_json(globals), args.format) {
            (true, _) | (_, SchemaDiffFormat::Json) => output::print_json(&diff),
            (false, SchemaDiffFormat::Text) => {
                print_diff(&diff);
                Ok(())
            }
        }
    }

    async fn schema_migrate(
        &mut self,
        globals: &GlobalArgs,
        options: &SchemaOptions,
        mut args: SchemaMigrateArgs,
    ) -> Result {
        let (project, client) = open_project(options)?;
        let source = String::from_utf8(project.read_schema()?)?;
        let diff = client.schema_diff(source.clone()).await?;
        if !diff.blocking.is_empty() {
            if !output::is_json(globals) {
                println!("Migration cannot be applied.");
                print_findings("Cannot apply", &diff.blocking);
            }
            return Err(CliError::with_details(
                "migration_blocked",
                "target schema constraints are not satisfied",
                output::json_value(&diff)?,
            )
            .into());
        }
        if !diff.destructive.is_empty() && !args.accept_data_loss {
            if globals.non_interactive || !io::stdin().is_terminal() || !io::stdout().is_terminal()
            {
                return Err(CliError::with_details(
                    "confirmation_required",
                    "migration would permanently delete data; review rad schema diff --format json and rerun with --accept-data-loss after explicit consent",
                    output::json_value(&diff.destructive)?,
                )
                .into());
            }
            args.accept_data_loss = confirm_data_loss(&diff)?;
            if !args.accept_data_loss {
                return Err(CliError::new("migration_cancelled", "migration cancelled").into());
            }
        }
        if !diff.changes.is_empty() {
            output::progress(globals, "Applying schema changes...");
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
            output::progress(
                globals,
                format!(
                    "Waiting for {} online schema transition(s)...",
                    migration.transition_ids.len()
                ),
            );
            migration = client.wait_for_migration(migration).await?;
        }
        let version = migration.schema_version;
        let hash = migration.schema_hash.clone();
        let transitions = migration.transition_ids.clone();
        let changes = migration.changes.clone();
        let store = StateStore::new(&project.state_dir);
        store.write_accepted(schema_state(migration))?;
        let generated = maybe_generate(&project, args.no_generate)?;
        let report = SchemaMigrateReport {
            state: "ready",
            schema_version: version,
            schema_hash: hash,
            changes,
            transition_ids: transitions,
            snapshot: store.snapshot_path(version as u64),
            lockfile: store.lock_path(),
            generated,
        };
        if output::is_json(globals) {
            output::print_json(&report)
        } else {
            print_migrate_report(&report);
            Ok(())
        }
    }

    async fn schema_pull(
        &mut self,
        globals: &GlobalArgs,
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
        let backup = if modified {
            let backup = store.backup_desired(&project.schema_file, Utc::now())?;
            Some(backup)
        } else {
            None
        };
        let version = server.schema_version;
        let hash = server.schema_hash.clone();
        let accepted = store.write_snapshot(server)?;
        StateStore::write_desired(&project.schema_file, &accepted.source)?;
        store.write_lock(&accepted.lock)?;
        let generated = maybe_generate(&project, args.no_generate)?;
        let report = SchemaPullReport {
            schema_version: version,
            schema_hash: hash,
            desired_schema: project.schema_file,
            lockfile: store.lock_path(),
            snapshot: store.snapshot_path(version as u64),
            backup,
            generated,
        };
        if output::is_json(globals) {
            output::print_json(&report)
        } else {
            print_pull_report(&report);
            Ok(())
        }
    }

    async fn schema_json_schema(
        &mut self,
        _globals: &GlobalArgs,
        _options: &SchemaOptions,
        _args: SchemaJsonSchemaArgs,
    ) -> Result {
        super::spec::print_schema();
        Ok(())
    }

    async fn schema_transitions_list(
        &mut self,
        globals: &GlobalArgs,
        options: &SchemaOptions,
        _transition_options: &SchemaTransitionsOptions,
        args: SchemaTransitionsListArgs,
    ) -> Result {
        let (_, client) = open_project(options)?;
        let kind = args.kind.map(transition_kind);
        let state = args.state.map(transition_state);
        let transitions = client.transitions(kind.as_ref(), state.as_ref()).await?;
        if output::is_json(globals) {
            output::print_json(&transitions)
        } else {
            print_transition_list(&transitions.transitions);
            Ok(())
        }
    }

    async fn schema_transitions_get(
        &mut self,
        globals: &GlobalArgs,
        options: &SchemaOptions,
        _transition_options: &SchemaTransitionsOptions,
        args: SchemaTransitionsGetArgs,
    ) -> Result {
        let (_, client) = open_project(options)?;
        let transition = client.transition(&args.transition).await?;
        print_transition(globals, &transition)
    }

    async fn schema_transitions_wait(
        &mut self,
        globals: &GlobalArgs,
        options: &SchemaOptions,
        _transition_options: &SchemaTransitionsOptions,
        args: SchemaTransitionsWaitArgs,
    ) -> Result {
        let interval = positive_duration(args.interval_ms, "--interval-ms", Duration::from_millis)?;
        let timeout = positive_duration(
            args.timeout_seconds,
            "--timeout-seconds",
            Duration::from_secs,
        )?;
        let (_, client) = open_project(options)?;
        let started = Instant::now();
        loop {
            let transition = client.transition(&args.transition).await?;
            if matches!(
                transition.state,
                TransitionState::Ready | TransitionState::Failed | TransitionState::Cancelled
            ) {
                if transition.state == TransitionState::Ready {
                    return print_transition(globals, &transition);
                }
                return Err(CliError::with_details(
                    "transition_terminal_failure",
                    format!(
                        "schema transition {:?} ended in state {}",
                        transition.transition_id, transition.state
                    ),
                    output::json_value(&transition)?,
                )
                .into());
            }
            if started.elapsed() >= timeout {
                return Err(CliError::with_details(
                    "transition_wait_timeout",
                    format!(
                        "timed out waiting for schema transition {:?}",
                        transition.transition_id
                    ),
                    output::json_value(&transition)?,
                )
                .into());
            }
            tokio::time::sleep(interval).await;
        }
    }

    async fn schema_transitions_cancel(
        &mut self,
        globals: &GlobalArgs,
        options: &SchemaOptions,
        _transition_options: &SchemaTransitionsOptions,
        args: SchemaTransitionsCancelArgs,
    ) -> Result {
        if !args.yes {
            if globals.non_interactive || !io::stdin().is_terminal() || !io::stdout().is_terminal()
            {
                return Err(CliError::new(
                    "confirmation_required",
                    "transition cancellation requires --yes in non-interactive mode",
                )
                .into());
            }
            if !confirm_transition_cancel(&args.transition)? {
                return Err(CliError::new(
                    "transition_cancellation_cancelled",
                    "transition cancellation cancelled",
                )
                .into());
            }
        }
        let (_, client) = open_project(options)?;
        let transition = client.cancel_transition(&args.transition).await?;
        print_transition(globals, &transition)
    }

    async fn generate(&mut self, globals: &GlobalArgs, args: GenerateArgs) -> Result {
        let target = super::project::GenerationTarget {
            language: match args.lang {
                super::generated::GenerateLang::Go => "go".into(),
            },
            output: args.out,
            package: args.pkg,
        };
        let generated = generate_target(&args.file, &target)?;
        if output::is_json(globals) {
            output::print_json(&serde_json::json!({ "generated": generated }))
        } else {
            print_generated(&generated);
            Ok(())
        }
    }

    async fn skills_list(
        &mut self,
        globals: &GlobalArgs,
        _options: &SkillsOptions,
        _args: SkillsListArgs,
    ) -> Result {
        super::skills::list(globals)
    }

    async fn skills_get(
        &mut self,
        globals: &GlobalArgs,
        _options: &SkillsOptions,
        args: SkillsGetArgs,
    ) -> Result {
        super::skills::get(globals, args.full)
    }

    async fn skills_path(
        &mut self,
        globals: &GlobalArgs,
        _options: &SkillsOptions,
        _args: SkillsPathArgs,
    ) -> Result {
        let path = super::skills::path()?;
        if output::is_json(globals) {
            output::print_json(&serde_json::json!({ "name": "rad", "path": path }))
        } else {
            println!("{}", path.display());
            Ok(())
        }
    }

    async fn spec(&mut self, globals: &GlobalArgs, args: SpecArgs) -> Result {
        super::spec::print_spec(globals, args)
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

fn print_migrate_report(report: &SchemaMigrateReport) {
    println!("Schema version {} committed.", report.schema_version);
    println!("Snapshot written:\n  {}", report.snapshot.display());
    println!("Lockfile updated:\n  {}", report.lockfile.display());
    print_generated(&report.generated);
}

fn print_pull_report(report: &SchemaPullReport) {
    if let Some(backup) = &report.backup {
        println!("Local schema backed up:\n  {}", backup.display());
    }
    println!("Pulled schema version {}.", report.schema_version);
    println!(
        "Updated:\n  {}\n  {}\n  {}",
        report.desired_schema.display(),
        report.lockfile.display(),
        report.snapshot.display()
    );
    print_generated(&report.generated);
}

fn print_generated(generated: &[GeneratedArtifact]) {
    for artifact in generated {
        println!(
            "generated {} ({} tables) for schema v{}",
            artifact.path.display(),
            artifact.tables,
            artifact.schema_version
        );
    }
}

fn transition_kind(kind: SchemaTransitionsListKind) -> TransitionKind {
    match kind {
        SchemaTransitionsListKind::IndexBuild => TransitionKind::IndexBuild,
        SchemaTransitionsListKind::ColumnReplacement => TransitionKind::ColumnReplacement,
        SchemaTransitionsListKind::ConstraintValidation => TransitionKind::ConstraintValidation,
    }
}

fn transition_state(state: SchemaTransitionsListState) -> TransitionState {
    match state {
        SchemaTransitionsListState::Waiting => TransitionState::Waiting,
        SchemaTransitionsListState::Building => TransitionState::Building,
        SchemaTransitionsListState::CatchingUp => TransitionState::CatchingUp,
        SchemaTransitionsListState::Validating => TransitionState::Validating,
        SchemaTransitionsListState::Ready => TransitionState::Ready,
        SchemaTransitionsListState::Failed => TransitionState::Failed,
        SchemaTransitionsListState::Cancelled => TransitionState::Cancelled,
    }
}

fn print_transition(globals: &GlobalArgs, transition: &TransitionControl) -> Result {
    if output::is_json(globals) {
        output::print_json(transition)
    } else {
        println!(
            "Transition {}\n  kind:      {}\n  object:    {}\n  state:     {}\n  pressure:  {}\n  scanned:   {}\n  delta lag: {}",
            transition.transition_id,
            transition.transition_kind,
            transition.object_id,
            transition.state,
            transition.retained_work_state,
            transition.rows_scanned,
            transition.delta_lag
        );
        if let Some(error) = &transition.last_error {
            println!("  error:     {error}");
        }
        Ok(())
    }
}

fn print_transition_list(transitions: &[TransitionControl]) {
    if transitions.is_empty() {
        println!("No schema transitions.");
        return;
    }
    for transition in transitions {
        println!(
            "{}  {:18} {:20} {}",
            transition.transition_id,
            transition.state,
            transition.transition_kind,
            transition.object_id
        );
    }
}

fn positive_duration(value: i64, flag: &str, construct: fn(u64) -> Duration) -> Result<Duration> {
    let value = u64::try_from(value).map_err(|_| format!("{flag} must be greater than zero"))?;
    if value == 0 {
        return Err(format!("{flag} must be greater than zero").into());
    }
    Ok(construct(value))
}

fn confirm_transition_cancel(transition: &str) -> Result<bool> {
    print!("Cancel schema transition {transition:?}? [y/N] ");
    io::stdout().flush()?;
    let mut answer = String::new();
    io::stdin().read_line(&mut answer)?;
    Ok(matches!(
        answer.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
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
    generated_match: bool,
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
        Some(0) if generated_match => "synchronized",
        Some(0) => "a generated client is missing or out of date",
        Some(_) => "rad.schema.yaml contains unapplied changes",
    }
}

fn generated_client_statuses(
    project: &Project,
    accepted: Option<&super::state::Accepted>,
) -> Vec<GeneratedClientStatus> {
    project
        .config
        .generate
        .iter()
        .map(|target| {
            let output = if target.output.is_absolute() {
                target.output.clone()
            } else {
                project.root.join(&target.output)
            };
            let path = match target.language.as_str() {
                "go" => output.join("rad_client_gen.go"),
                _ => output,
            };
            let (version, hash, error) = match std::fs::read_to_string(&path) {
                Ok(source) => {
                    let version = source.lines().find_map(|line| {
                        line.trim()
                            .strip_prefix("const SchemaVersion uint64 = ")?
                            .parse()
                            .ok()
                    });
                    let hash = source.lines().find_map(|line| {
                        line.trim()
                            .strip_prefix("const SchemaHash = \"")?
                            .strip_suffix('"')
                            .map(str::to_owned)
                    });
                    let error = if version.is_none() || hash.is_none() {
                        Some("schema identity constants are missing; run rad generate".into())
                    } else {
                        None
                    };
                    (version, hash, error)
                }
                Err(read_error) => (
                    None,
                    None,
                    Some(if read_error.kind() == io::ErrorKind::NotFound {
                        "file is missing; run rad generate".into()
                    } else {
                        format!("could not read file: {read_error}")
                    }),
                ),
            };
            let state = match (accepted, version, hash.as_deref(), error.is_none()) {
                (Some(accepted), Some(version), Some(hash), true)
                    if accepted.lock.schema_version == version
                        && accepted.lock.schema_hash == hash =>
                {
                    "synchronized"
                }
                (None, _, _, _) => "unverifiable",
                _ => "out_of_date",
            };
            GeneratedClientStatus {
                language: target.language.clone(),
                path,
                state,
                schema_version: version,
                schema_hash: hash,
                error,
            }
        })
        .collect()
}

fn maybe_generate(project: &Project, disabled: bool) -> Result<Vec<GeneratedArtifact>> {
    if disabled {
        return Ok(Vec::new());
    }
    let mut generated = Vec::new();
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
        generated.extend(generate_target(&project.schema_file, &target)?);
    }
    Ok(generated)
}

fn generate_target(
    schema_file: &Path,
    target: &super::project::GenerationTarget,
) -> Result<Vec<GeneratedArtifact>> {
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
    let schema_version = accepted.lock.schema_version;
    let schema_hash = accepted.lock.schema_hash.clone();
    let files = crate::codegen::generate(
        &target.language,
        &schema,
        &crate::codegen::Options {
            package: target.package.clone(),
            schema_source: accepted.source,
            schema_version,
            schema_hash,
        },
    )?;
    let paths = crate::codegen::publish(&target.output, &target.language, files)?;
    Ok(paths
        .into_iter()
        .map(|path| GeneratedArtifact {
            language: target.language.clone(),
            path: target.output.join(path),
            tables: schema.tables.len(),
            schema_version,
        })
        .collect())
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
