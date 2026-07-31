use std::path::PathBuf;

use serde::Serialize;
use serde_json::{Value, json};

use super::client::Client;
use super::generated::{DoctorArgs, GlobalArgs};
use super::output::{self, CliError};
use super::project::Project;
use super::state::StateStore;
use crate::http::generated::types::{DatabaseInfo, TransitionState, TransitionWorkState};
use crate::process::Result;

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
enum CheckStatus {
    Pass,
    Warning,
    Fail,
}

#[derive(Debug, Serialize)]
struct Check {
    name: &'static str,
    status: CheckStatus,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    details: Option<Value>,
}

#[derive(Debug, Serialize)]
struct Report {
    healthy: bool,
    checks: Vec<Check>,
}

impl Report {
    fn push(&mut self, name: &'static str, status: CheckStatus, message: impl Into<String>) {
        self.checks.push(Check {
            name,
            status,
            message: message.into(),
            details: None,
        });
    }

    fn push_details(
        &mut self,
        name: &'static str,
        status: CheckStatus,
        message: impl Into<String>,
        details: Value,
    ) {
        self.checks.push(Check {
            name,
            status,
            message: message.into(),
            details: Some(details),
        });
    }

    fn finish(&mut self) {
        self.healthy = !self
            .checks
            .iter()
            .any(|check| matches!(check.status, CheckStatus::Fail));
    }

    fn text(&self) -> String {
        let mut output = String::new();
        for check in &self.checks {
            let label = match check.status {
                CheckStatus::Pass => "PASS",
                CheckStatus::Warning => "WARN",
                CheckStatus::Fail => "FAIL",
            };
            output.push_str(&format!("{label:4}  {:18} {}\n", check.name, check.message));
        }
        output.push_str(if self.healthy {
            "\nDoctor: healthy"
        } else {
            "\nDoctor: failed"
        });
        output
    }
}

pub(super) async fn run(globals: &GlobalArgs, args: DoctorArgs) -> Result {
    let report = diagnose(args).await;
    if !report.healthy {
        return Err(CliError::with_details(
            "doctor_failed",
            "one or more Rad diagnostics failed",
            output::json_value(&report)?,
        )
        .with_text(report.text())
        .into());
    }
    if output::is_json(globals) {
        output::print_json(&report)
    } else {
        println!("{}", report.text());
        Ok(())
    }
}

async fn diagnose(args: DoctorArgs) -> Report {
    let mut report = Report {
        healthy: false,
        checks: Vec::new(),
    };

    let project = match Project::load(&args.config, &args.file) {
        Ok(project) => {
            report.push(
                "configuration",
                CheckStatus::Pass,
                format!("loaded {}", args.config.display()),
            );
            project
        }
        Err(error) => {
            report.push("configuration", CheckStatus::Fail, error.to_string());
            report.finish();
            return report;
        }
    };

    let desired = match project.read_schema() {
        Ok(source) => match crate::engine::catalog::schema::parse(
            &project.schema_file.display().to_string(),
            &source,
        ) {
            Ok(schema) => {
                let columns = schema
                    .tables
                    .iter()
                    .map(|table| table.def.columns.len())
                    .sum::<usize>();
                report.push_details(
                    "desired-schema",
                    CheckStatus::Pass,
                    format!("{} tables, {columns} columns", schema.tables.len()),
                    json!({ "path": project.schema_file, "tables": schema.tables.len(), "columns": columns }),
                );
                Some(source)
            }
            Err(error) => {
                report.push("desired-schema", CheckStatus::Fail, error.to_string());
                None
            }
        },
        Err(error) => {
            report.push("desired-schema", CheckStatus::Fail, error.to_string());
            None
        }
    };

    let store = StateStore::new(&project.state_dir);
    let accepted = match store.load() {
        Ok(accepted) => {
            report.push_details(
                "accepted-state",
                CheckStatus::Pass,
                format!("schema version {}", accepted.lock.schema_version),
                json!({
                    "lockfile": store.lock_path(),
                    "version": accepted.lock.schema_version,
                    "hash": accepted.lock.schema_hash,
                    "snapshot": accepted.lock.snapshot,
                }),
            );
            Some(accepted)
        }
        Err(error) => {
            report.push(
                "accepted-state",
                CheckStatus::Warning,
                format!("{}; run rad schema migrate or rad schema pull", error),
            );
            None
        }
    };

    let client = match Client::connect(&project.config.database_url) {
        Ok(client) => client,
        Err(error) => {
            report.push("server", CheckStatus::Fail, error.to_string());
            check_generated(&mut report, &project, accepted.as_ref());
            report.finish();
            return report;
        }
    };

    match client.health().await {
        Ok(health) if health.status == "ok" => report.push_details(
            "server-health",
            CheckStatus::Pass,
            format!("reachable in {} mode", health.mode),
            output::json_value(&health).unwrap_or(Value::Null),
        ),
        Ok(health) => report.push_details(
            "server-health",
            CheckStatus::Fail,
            format!("unexpected health status {:?}", health.status),
            output::json_value(&health).unwrap_or(Value::Null),
        ),
        Err(error) => report.push("server-health", CheckStatus::Fail, error.to_string()),
    }

    let info = match client.info().await {
        Ok(info) => {
            report.push_details(
                "server-schema",
                CheckStatus::Pass,
                format!("version {} ({})", info.schema_version, info.mode),
                output::json_value(&info).unwrap_or(Value::Null),
            );
            Some(info)
        }
        Err(error) => {
            report.push("server-schema", CheckStatus::Fail, error.to_string());
            None
        }
    };

    compare_accepted(&mut report, accepted.as_ref(), info.as_ref());

    if let Some(desired) = desired {
        match String::from_utf8(desired) {
            Ok(source) => match client.schema_diff(source).await {
                Ok(diff) => {
                    let status = if !diff.blocking.is_empty() {
                        CheckStatus::Fail
                    } else if !diff.destructive.is_empty() || !diff.changes.is_empty() {
                        CheckStatus::Warning
                    } else {
                        CheckStatus::Pass
                    };
                    report.push_details(
                        "schema-diff",
                        status,
                        format!(
                            "{} changes, {} destructive, {} blocking",
                            diff.changes.len(),
                            diff.destructive.len(),
                            diff.blocking.len()
                        ),
                        output::json_value(&diff).unwrap_or(Value::Null),
                    );
                }
                Err(error) => report.push("schema-diff", CheckStatus::Fail, error.to_string()),
            },
            Err(error) => report.push("schema-diff", CheckStatus::Fail, error.to_string()),
        }
    }

    match client.transitions(None, None).await {
        Ok(transitions) => {
            let failed = transitions
                .transitions
                .iter()
                .filter(|transition| transition.state == TransitionState::Failed)
                .count();
            let active = transitions
                .transitions
                .iter()
                .filter(|transition| {
                    !matches!(
                        transition.state,
                        TransitionState::Ready
                            | TransitionState::Failed
                            | TransitionState::Cancelled
                    )
                })
                .count();
            let gated = transitions.transitions.iter().any(|transition| {
                transition.retained_work_state == TransitionWorkState::WriteGated
            });
            let status = if gated {
                CheckStatus::Fail
            } else if failed > 0 || active > 0 {
                CheckStatus::Warning
            } else {
                CheckStatus::Pass
            };
            report.push_details(
                "transitions",
                status,
                format!("{active} active, {failed} failed, write_gated={gated}"),
                output::json_value(&transitions).unwrap_or(Value::Null),
            );
        }
        Err(error) => report.push("transitions", CheckStatus::Fail, error.to_string()),
    }

    check_generated(&mut report, &project, accepted.as_ref());
    report.finish();
    report
}

fn compare_accepted(
    report: &mut Report,
    accepted: Option<&super::state::Accepted>,
    info: Option<&DatabaseInfo>,
) {
    let (Some(accepted), Some(info)) = (accepted, info) else {
        return;
    };
    let Ok(server_version) = u64::try_from(info.schema_version) else {
        report.push(
            "schema-identity",
            CheckStatus::Fail,
            format!(
                "server returned invalid schema version {}",
                info.schema_version
            ),
        );
        return;
    };
    let (status, message): (CheckStatus, String) = if accepted.lock.schema_version == server_version
        && accepted.lock.schema_hash == info.schema_hash
    {
        (CheckStatus::Pass, "server and accepted state agree".into())
    } else if accepted.lock.schema_version == server_version {
        (CheckStatus::Fail, "schema history diverged".into())
    } else if accepted.lock.schema_version < server_version {
        (
            CheckStatus::Warning,
            "server is ahead; run rad schema pull when the server is authoritative".into(),
        )
    } else {
        (
            CheckStatus::Warning,
            "local accepted state is ahead of the server".into(),
        )
    };
    report.push("schema-identity", status, message);
}

fn check_generated(
    report: &mut Report,
    project: &Project,
    accepted: Option<&super::state::Accepted>,
) {
    if project.config.generate.is_empty() {
        report.push(
            "generated-client",
            CheckStatus::Pass,
            "no configured generation targets",
        );
        return;
    }
    let Some(accepted) = accepted else {
        report.push(
            "generated-client",
            CheckStatus::Warning,
            "cannot verify clients without accepted state",
        );
        return;
    };
    for target in &project.config.generate {
        let output = if target.output.is_absolute() {
            target.output.clone()
        } else {
            project.root.join(&target.output)
        };
        let path = generated_path(&output, &target.language);
        match std::fs::read_to_string(&path) {
            Ok(source)
                if source.contains(&format!(
                    "const SchemaVersion uint64 = {}",
                    accepted.lock.schema_version
                )) && source.contains(&format!(
                    "const SchemaHash = {:?}",
                    accepted.lock.schema_hash
                )) =>
            {
                report.push(
                    "generated-client",
                    CheckStatus::Pass,
                    format!("{} matches accepted state", path.display()),
                );
            }
            Ok(_) => report.push(
                "generated-client",
                CheckStatus::Fail,
                format!(
                    "{} does not match accepted state; run rad generate",
                    path.display()
                ),
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => report.push(
                "generated-client",
                CheckStatus::Warning,
                format!("{} is missing; run rad generate", path.display()),
            ),
            Err(error) => report.push(
                "generated-client",
                CheckStatus::Fail,
                format!("could not read {}: {error}", path.display()),
            ),
        }
    }
}

fn generated_path(output: &std::path::Path, language: &str) -> PathBuf {
    match language {
        "go" => output.join("rad_client_gen.go"),
        _ => output.to_owned(),
    }
}
