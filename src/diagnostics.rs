use std::fmt;
use std::str::FromStr;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use serde_json::{Map, Value, json};

use crate::engine::exec::{Error, Program, ProgramOptions, ProgramResult};
use crate::protocol::generated::pir;

pub const HEADER: &str = "rad-diagnostics";
pub const FORMAT: &str = "rad-program-diagnostics-v1";
pub const MAX_DOCUMENT_BYTES: usize = 1024 * 1024;

#[derive(Clone, Copy, Debug, Default, Eq, Ord, PartialEq, PartialOrd)]
#[repr(u8)]
pub enum Level {
    Off,
    #[default]
    Summary,
    Detailed,
    Full,
}

impl Level {
    pub const fn captures_plans(self) -> bool {
        matches!(self, Self::Detailed | Self::Full)
    }
}

impl fmt::Display for Level {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Off => "off",
            Self::Summary => "summary",
            Self::Detailed => "detailed",
            Self::Full => "full",
        })
    }
}

impl FromStr for Level {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "off" => Ok(Self::Off),
            "summary" => Ok(Self::Summary),
            "detailed" => Ok(Self::Detailed),
            "full" => Ok(Self::Full),
            _ => Err(format!(
                "unknown diagnostic level {value:?} (off, summary, detailed, or full)"
            )),
        }
    }
}

static MAX_LEVEL: AtomicU8 = AtomicU8::new(Level::Summary as u8);

pub fn set_max_level(level: Level) {
    MAX_LEVEL.store(level as u8, Ordering::Release);
}

pub fn max_level() -> Level {
    match MAX_LEVEL.load(Ordering::Acquire) {
        0 => Level::Off,
        1 => Level::Summary,
        2 => Level::Detailed,
        _ => Level::Full,
    }
}

#[derive(Clone, Debug)]
pub struct ProgramDiagnosticRecorder {
    level: Level,
    inner: Arc<Mutex<State>>,
}

impl ProgramDiagnosticRecorder {
    pub fn new(level: Level) -> Self {
        Self {
            level,
            inner: Arc::new(Mutex::new(State::default())),
        }
    }

    pub const fn level(&self) -> Level {
        self.level
    }

    pub fn submitted(&self, program: &pir::Program) {
        if self.level != Level::Full {
            return;
        }
        let Ok(document) = serde_json::to_vec(program) else {
            return;
        };
        let Some(mut state) = self.inner.lock().ok() else {
            return;
        };
        if document.len() <= crate::engine::exec::diagnostic::MAX_PROGRAM_DOCUMENT_BYTES {
            state.submitted = serde_json::from_slice(&document).ok();
        } else {
            state.submitted_omitted_bytes = Some(document.len());
        }
    }

    pub fn start(
        &self,
        program: &Program,
        options: &ProgramOptions,
        fingerprints: &crate::engine::exec::diagnostic::ProgramFingerprints,
    ) {
        let exact = (self.level == Level::Full)
            .then(|| bounded_document(crate::engine::exec::diagnostic::program_document(program)));
        let family = self.level.captures_plans().then(|| {
            bounded_document(crate::engine::exec::diagnostic::program_family_document(
                program,
            ))
        });
        let statements = program
            .statements
            .iter()
            .map(|statement| {
                json!({
                    "name": statement.name(),
                    "kind": statement.kind(),
                    "status": "pending",
                })
            })
            .collect();
        let Some(mut state) = self.inner.lock().ok() else {
            return;
        };
        state.started = Some(Instant::now());
        state.program_fingerprint = Some(fingerprints.family.clone());
        state.exact_program_fingerprint =
            (self.level == Level::Full).then(|| fingerprints.exact.clone());
        state.statements = statements;
        state.dry_run = options.dry_run;
        if let Some(document) = exact {
            state.lowered = document.value;
            state.lowered_omitted_bytes = document.omitted_bytes;
        }
        if let Some(document) = family {
            state.family = document.value;
            state.family_omitted_bytes = document.omitted_bytes;
        }
    }

    pub fn finish(&self, result: &Result<ProgramResult, Error>) {
        let Some(mut state) = self.inner.lock().ok() else {
            return;
        };
        state.duration_micros = state
            .started
            .map_or(0, |started| elapsed_micros(started.elapsed()));
        match result {
            Ok(result) => {
                state.status = Some("success");
                state.result_rows = result_rows(&result.result);
                state.affected_rows = result
                    .statements
                    .iter()
                    .map(|statement| statement.affected as u64)
                    .sum();
                for plan in &result.plans {
                    if let Some(measurement) = &plan.measurement {
                        state.planning_micros = state
                            .planning_micros
                            .saturating_add(measurement.planning_micros);
                        state.execution_micros = state
                            .execution_micros
                            .saturating_add(measurement.execution_micros);
                        add_kv(&mut state.kv, measurement.logical_kv);
                    }
                }
                for statement in &result.statements {
                    if let Some(Value::Object(fields)) = state.statements.iter_mut().find(|value| {
                        value.get("name").and_then(Value::as_str) == Some(statement.name.as_str())
                    }) {
                        fields.insert("status".into(), Value::String("success".into()));
                        fields.insert("affectedRows".into(), (statement.affected as u64).into());
                    }
                }
                if self.level.captures_plans() {
                    state.plans = serde_json::to_value(&result.plans).ok();
                }
            }
            Err(error) => {
                state.status = Some("error");
                state.error_kind = Some(error.kind().as_str());
                state.error_reason = Some(error.reason().as_str());
            }
        }
    }

    pub fn document(&self, context: &crate::logging::RequestContext, http_status: u16) -> Value {
        let Some(state) = self.inner.lock().ok() else {
            return json!({
                "format": FORMAT,
                "requestId": context.request_id,
                "traceId": context.trace_id,
                "status": status_from_http(http_status),
                "truncated": true,
            });
        };
        let mut document = Map::from_iter([
            ("format".into(), Value::String(FORMAT.into())),
            ("level".into(), Value::String(self.level.to_string())),
            (
                "requestId".into(),
                Value::String(context.request_id.clone()),
            ),
            ("traceId".into(), Value::String(context.trace_id.clone())),
            (
                "status".into(),
                Value::String(
                    state
                        .status
                        .unwrap_or_else(|| status_from_http(http_status))
                        .into(),
                ),
            ),
            ("durationUs".into(), state.duration_micros.into()),
            ("dryRun".into(), state.dry_run.into()),
            ("resultRows".into(), state.result_rows.into()),
            ("affectedRows".into(), state.affected_rows.into()),
            ("planningUs".into(), state.planning_micros.into()),
            ("executionUs".into(), state.execution_micros.into()),
            (
                "kv".into(),
                serde_json::to_value(state.kv).unwrap_or(Value::Null),
            ),
            ("statements".into(), Value::Array(state.statements.clone())),
        ]);
        insert_option(
            &mut document,
            "programFingerprint",
            &state.program_fingerprint,
        );
        insert_option(
            &mut document,
            "exactProgramFingerprint",
            &state.exact_program_fingerprint,
        );
        insert_option_str(&mut document, "errorKind", state.error_kind);
        insert_option_str(&mut document, "errorReason", state.error_reason);
        insert_value(&mut document, "submittedProgram", &state.submitted);
        insert_value(&mut document, "loweredProgram", &state.lowered);
        insert_value(&mut document, "loweredProgramFamily", &state.family);
        insert_value(&mut document, "plans", &state.plans);
        let omitted = json!({
            "submittedProgramBytes": state.submitted_omitted_bytes,
            "loweredProgramBytes": state.lowered_omitted_bytes,
            "loweredProgramFamilyBytes": state.family_omitted_bytes,
        });
        if omitted
            .as_object()
            .is_some_and(|fields| fields.values().any(|value| !value.is_null()))
        {
            document.insert("omitted".into(), omitted);
        }
        let mut value = Value::Object(document);
        if serde_json::to_vec(&value).is_ok_and(|bytes| bytes.len() > MAX_DOCUMENT_BYTES) {
            let fields = value
                .as_object_mut()
                .expect("diagnostic document is an object");
            fields.remove("submittedProgram");
            fields.remove("loweredProgram");
            fields.remove("loweredProgramFamily");
            fields.remove("plans");
            fields.insert("truncated".into(), true.into());
        }
        value
    }
}

#[derive(Debug, Default)]
struct State {
    started: Option<Instant>,
    duration_micros: u64,
    status: Option<&'static str>,
    program_fingerprint: Option<String>,
    exact_program_fingerprint: Option<String>,
    statements: Vec<Value>,
    dry_run: bool,
    result_rows: u64,
    affected_rows: u64,
    planning_micros: u64,
    execution_micros: u64,
    kv: crate::engine::exec::observe::KvWork,
    error_kind: Option<&'static str>,
    error_reason: Option<&'static str>,
    submitted: Option<Value>,
    submitted_omitted_bytes: Option<usize>,
    lowered: Option<Value>,
    lowered_omitted_bytes: Option<usize>,
    family: Option<Value>,
    family_omitted_bytes: Option<usize>,
    plans: Option<Value>,
}

struct BoundedDocument {
    value: Option<Value>,
    omitted_bytes: Option<usize>,
}

fn bounded_document(document: Vec<u8>) -> BoundedDocument {
    if document.len() <= crate::engine::exec::diagnostic::MAX_PROGRAM_DOCUMENT_BYTES {
        BoundedDocument {
            value: serde_json::from_slice(&document).ok(),
            omitted_bytes: None,
        }
    } else {
        BoundedDocument {
            value: None,
            omitted_bytes: Some(document.len()),
        }
    }
}

fn elapsed_micros(duration: std::time::Duration) -> u64 {
    duration.as_micros().min(u128::from(u64::MAX)) as u64
}

fn add_kv(
    total: &mut crate::engine::exec::observe::KvWork,
    value: crate::engine::exec::observe::KvWork,
) {
    total.gets = total.gets.saturating_add(value.gets);
    total.puts = total.puts.saturating_add(value.puts);
    total.deletes = total.deletes.saturating_add(value.deletes);
    total.scans = total.scans.saturating_add(value.scans);
    total.forward_seeks = total.forward_seeks.saturating_add(value.forward_seeks);
    total.iterated = total.iterated.saturating_add(value.iterated);
    total.bytes_read = total.bytes_read.saturating_add(value.bytes_read);
    total.bytes_written = total.bytes_written.saturating_add(value.bytes_written);
}

fn result_rows(result: &crate::engine::lir::Datum) -> u64 {
    match result {
        crate::engine::lir::Datum::Null => 0,
        crate::engine::lir::Datum::Array(rows) => rows.len() as u64,
        crate::engine::lir::Datum::Scalar(_) | crate::engine::lir::Datum::Object(_) => 1,
    }
}

fn status_from_http(status: u16) -> &'static str {
    if status < 400 { "success" } else { "error" }
}

fn insert_option(fields: &mut Map<String, Value>, name: &str, value: &Option<String>) {
    if let Some(value) = value {
        fields.insert(name.into(), Value::String(value.clone()));
    }
}

fn insert_option_str(fields: &mut Map<String, Value>, name: &str, value: Option<&str>) {
    if let Some(value) = value {
        fields.insert(name.into(), Value::String(value.into()));
    }
}

fn insert_value(fields: &mut Map<String, Value>, name: &str, value: &Option<Value>) {
    if let Some(value) = value {
        fields.insert(name.into(), value.clone());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn levels_parse_and_order() {
        assert_eq!("off".parse(), Ok(Level::Off));
        assert_eq!("summary".parse(), Ok(Level::Summary));
        assert_eq!("detailed".parse(), Ok(Level::Detailed));
        assert_eq!("full".parse(), Ok(Level::Full));
        assert!("trace".parse::<Level>().is_err());
        assert!(Level::Full > Level::Detailed);
    }
}
