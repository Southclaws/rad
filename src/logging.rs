use std::env;
use std::fmt;
use std::future::Future;
use std::io::Write as _;
use std::str::FromStr;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tracing_subscriber::filter::{LevelFilter, Targets};
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::fmt::time::UtcTime;
use tracing_subscriber::prelude::*;

static INSTALLED: AtomicBool = AtomicBool::new(false);
static TELEMETRY: Mutex<Option<crate::telemetry::Runtime>> = Mutex::new(None);

#[derive(Clone, Debug, Default)]
pub struct RequestContext {
    pub transport: &'static str,
    pub request_id: String,
    pub transaction_id: String,
    pub client_ip: String,
    pub application_name: String,
    pub transaction_state: &'static str,
    pub trace_id: String,
    pub span_id: String,
    pub diagnostics: Option<crate::diagnostics::ProgramDiagnosticRecorder>,
    pub parent_span: Option<tracing::Span>,
}

tokio::task_local! {
    static REQUEST_CONTEXT: RequestContext;
}

pub async fn with_request_context<T>(
    context: RequestContext,
    future: impl Future<Output = T>,
) -> T {
    REQUEST_CONTEXT.scope(context, future).await
}

pub fn request_context() -> RequestContext {
    REQUEST_CONTEXT.try_with(Clone::clone).unwrap_or_default()
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Level {
    Error,
    Warn,
    #[default]
    Info,
    Debug,
}

impl Level {
    fn filter(self) -> LevelFilter {
        match self {
            Self::Error => LevelFilter::ERROR,
            Self::Warn => LevelFilter::WARN,
            Self::Info => LevelFilter::INFO,
            Self::Debug => LevelFilter::DEBUG,
        }
    }
}

impl fmt::Display for Level {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Error => "error",
            Self::Warn => "warn",
            Self::Info => "info",
            Self::Debug => "debug",
        })
    }
}

impl FromStr for Level {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "error" => Ok(Self::Error),
            "warn" => Ok(Self::Warn),
            "info" => Ok(Self::Info),
            "debug" => Ok(Self::Debug),
            _ => Err(format!(
                "unknown log level {value:?} (error, warn, info, or debug)"
            )),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Format {
    #[default]
    Text,
    Json,
    Logfmt,
}

impl fmt::Display for Format {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Text => "text",
            Self::Json => "json",
            Self::Logfmt => "logfmt",
        })
    }
}

impl FromStr for Format {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "text" => Ok(Self::Text),
            "json" => Ok(Self::Json),
            "logfmt" => Ok(Self::Logfmt),
            _ => Err(format!(
                "unknown log format {value:?} (text, json, or logfmt)"
            )),
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Config {
    pub level: Level,
    pub format: Format,
    pub programs: bool,
    pub telemetry: crate::telemetry::Config,
    pub diagnostics: crate::diagnostics::Level,
}

impl Config {
    pub fn from_env() -> Result<Self, String> {
        Ok(Self {
            level: env::var("RAD_LOG_LEVEL")
                .unwrap_or_else(|_| "info".to_owned())
                .parse()?,
            format: env::var("RAD_LOG_FORMAT")
                .unwrap_or_else(|_| "text".to_owned())
                .parse()?,
            programs: parse_bool_env("RAD_LOG_PROGRAMS")?,
            telemetry: crate::telemetry::Config {
                endpoint: env::var("OTEL_EXPORTER_OTLP_ENDPOINT")
                    .ok()
                    .filter(|value| !value.is_empty()),
                instance_id: env::var("RAD_INSTANCE_ID")
                    .ok()
                    .filter(|value| !value.is_empty()),
                role: env::var("RAD_ROLE").ok().filter(|value| !value.is_empty()),
                metrics: parse_bool_env_with_default("RAD_METRICS", true)?,
            },
            diagnostics: env::var("RAD_DIAGNOSTICS")
                .unwrap_or_else(|_| "summary".to_owned())
                .parse()?,
        })
    }

    fn filter(&self) -> Targets {
        let program_level = self.program_filter();
        Targets::new()
            .with_default(LevelFilter::OFF)
            .with_target("rad", self.level.filter())
            .with_target("rad::program", program_level)
    }

    fn program_filter(&self) -> LevelFilter {
        if self.level == Level::Debug {
            LevelFilter::DEBUG
        } else if self.programs {
            LevelFilter::INFO
        } else {
            LevelFilter::OFF
        }
    }
}

pub fn install(config: Config) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let telemetry = crate::telemetry::Runtime::build(&config.telemetry)?;
    let telemetry_layer = telemetry
        .as_ref()
        .and_then(|runtime| runtime.tracer())
        .map(|tracer| {
            tracing_opentelemetry::layer()
                .with_tracer(tracer)
                .with_filter(
                    Targets::new()
                        .with_default(LevelFilter::OFF)
                        .with_target("rad::telemetry", LevelFilter::DEBUG),
                )
        });
    let result = match config.format {
        Format::Text => tracing_subscriber::registry()
            .with(telemetry_layer)
            .with(
                tracing_subscriber::fmt::layer()
                    .with_ansi(false)
                    .with_target(false)
                    .with_writer(StderrEventWriter::new(Format::Text))
                    .with_timer(UtcTime::rfc_3339())
                    .with_filter(config.filter()),
            )
            .try_init(),
        Format::Json => tracing_subscriber::registry()
            .with(telemetry_layer)
            .with(
                tracing_subscriber::fmt::layer()
                    .json()
                    .flatten_event(true)
                    .with_current_span(false)
                    .with_span_list(false)
                    .with_target(false)
                    .with_writer(StderrEventWriter::new(Format::Json))
                    .with_timer(UtcTime::rfc_3339())
                    .with_filter(config.filter()),
            )
            .try_init(),
        Format::Logfmt => tracing_subscriber::registry()
            .with(telemetry_layer)
            .with(
                tracing_logfmt::builder()
                    .with_target(false)
                    .with_span_name(false)
                    .with_span_path(false)
                    .layer()
                    .with_writer(StderrEventWriter::new(Format::Logfmt))
                    .with_filter(config.filter()),
            )
            .try_init(),
    };
    result?;
    if let Some(telemetry) = telemetry {
        telemetry.activate();
        *TELEMETRY.lock().expect("telemetry runtime lock poisoned") = Some(telemetry);
    }
    crate::diagnostics::set_max_level(config.diagnostics);
    INSTALLED.store(true, Ordering::Release);
    Ok(())
}

pub fn shutdown() {
    let telemetry = TELEMETRY
        .lock()
        .expect("telemetry runtime lock poisoned")
        .take();
    if let Some(telemetry) = telemetry {
        telemetry.shutdown(Duration::from_secs(5));
    }
}

#[derive(Clone, Copy)]
struct StderrEventWriter {
    format: Format,
}

impl StderrEventWriter {
    const fn new(format: Format) -> Self {
        Self { format }
    }
}

impl<'a> MakeWriter<'a> for StderrEventWriter {
    type Writer = BufferedEvent;

    fn make_writer(&'a self) -> Self::Writer {
        BufferedEvent {
            format: self.format,
            bytes: Vec::new(),
        }
    }
}

struct BufferedEvent {
    format: Format,
    bytes: Vec<u8>,
}

impl std::io::Write for BufferedEvent {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        self.bytes.extend_from_slice(buffer);
        Ok(buffer.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Drop for BufferedEvent {
    fn drop(&mut self) {
        if self.bytes.is_empty() {
            return;
        }
        let output = String::from_utf8_lossy(&self.bytes);
        let output = match self.format {
            Format::Json => lowercase_json_level(&output),
            Format::Logfmt => output.replacen("ts=", "timestamp=", 1),
            Format::Text => lowercase_text_level(&output),
        };
        let mut stderr = std::io::stderr().lock();
        let _ = stderr.write_all(output.as_bytes());
    }
}

fn lowercase_json_level(line: &str) -> String {
    [
        ("\"level\":\"ERROR\"", "\"level\":\"error\""),
        ("\"level\":\"WARN\"", "\"level\":\"warn\""),
        ("\"level\":\"INFO\"", "\"level\":\"info\""),
        ("\"level\":\"DEBUG\"", "\"level\":\"debug\""),
    ]
    .into_iter()
    .fold(line.to_owned(), |line, (from, to)| {
        line.replacen(from, to, 1)
    })
}

fn lowercase_text_level(line: &str) -> String {
    [
        (" ERROR ", " error "),
        ("  WARN ", "  warn "),
        ("  INFO ", "  info "),
        (" DEBUG ", " debug "),
    ]
    .into_iter()
    .fold(line.to_owned(), |line, (from, to)| {
        line.replacen(from, to, 1)
    })
}

pub fn is_installed() -> bool {
    INSTALLED.load(Ordering::Acquire)
}

pub fn terminal_failure(error: &(dyn std::error::Error + 'static)) {
    let terminal = error
        .downcast_ref::<std::io::Error>()
        .and_then(std::io::Error::get_ref)
        .and_then(|source| source.downcast_ref::<crate::process::TerminalRuntimeError>());
    tracing::error!(
        target: "rad",
        event = "process.failed",
        component = "process",
        error_kind = "terminal",
        error_reason = terminal.map_or_else(|| stable_error_reason(error), |error| error.reason),
        message = terminal.map_or("Rad stopped because of a terminal error", |error| error.message)
    );
}

fn parse_bool_env(name: &str) -> Result<bool, String> {
    let value = env::var(name).ok();
    parse_bool_value(name, value.as_deref())
}

fn parse_bool_env_with_default(name: &str, default: bool) -> Result<bool, String> {
    let value = env::var(name).ok();
    parse_bool_value_with_default(name, value.as_deref(), default)
}

fn parse_bool_value_with_default(
    name: &str,
    value: Option<&str>,
    default: bool,
) -> Result<bool, String> {
    match value {
        None | Some("") => Ok(default),
        value => parse_bool_value(name, value),
    }
}

fn parse_bool_value(name: &str, value: Option<&str>) -> Result<bool, String> {
    match value {
        None | Some("") | Some("0" | "false" | "no") => Ok(false),
        Some("1" | "true" | "yes") => Ok(true),
        Some(value) => Err(format!(
            "invalid {name} value {value:?} (true, false, 1, 0, yes, or no)"
        )),
    }
}

pub fn stable_error_reason(error: &(dyn std::error::Error + 'static)) -> &'static str {
    if error.downcast_ref::<std::io::Error>().is_some() {
        "io"
    } else {
        "internal"
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;

    #[test]
    fn parses_levels() {
        assert_eq!("error".parse(), Ok(Level::Error));
        assert_eq!("warn".parse(), Ok(Level::Warn));
        assert_eq!("info".parse(), Ok(Level::Info));
        assert_eq!("debug".parse(), Ok(Level::Debug));
        assert!("trace".parse::<Level>().is_err());
    }

    #[test]
    fn parses_formats() {
        assert_eq!("text".parse(), Ok(Format::Text));
        assert_eq!("json".parse(), Ok(Format::Json));
        assert_eq!("logfmt".parse(), Ok(Format::Logfmt));
        assert!("pretty".parse::<Format>().is_err());
    }

    #[test]
    fn logging_defaults_and_boolean_values_are_stable() {
        assert_eq!(Config::default().level, Level::Info);
        assert_eq!(Config::default().format, Format::Text);
        assert!(!Config::default().programs);
        assert_eq!(
            Config::default().diagnostics,
            crate::diagnostics::Level::Summary
        );
        assert_eq!(
            Config::default().telemetry,
            crate::telemetry::Config::default()
        );
        for value in [None, Some(""), Some("0"), Some("false"), Some("no")] {
            assert_eq!(parse_bool_value("RAD_LOG_PROGRAMS", value), Ok(false));
        }
        for value in [Some("1"), Some("true"), Some("yes")] {
            assert_eq!(parse_bool_value("RAD_LOG_PROGRAMS", value), Ok(true));
        }
        assert!(parse_bool_value("RAD_LOG_PROGRAMS", Some("on")).is_err());
        for value in [None, Some(""), Some("1"), Some("true"), Some("yes")] {
            assert_eq!(
                parse_bool_value_with_default("RAD_METRICS", value, true),
                Ok(true)
            );
        }
        for value in [Some("0"), Some("false"), Some("no")] {
            assert_eq!(
                parse_bool_value_with_default("RAD_METRICS", value, true),
                Ok(false)
            );
        }
    }

    #[test]
    fn program_filter_is_independent_from_operational_level() {
        for level in [Level::Error, Level::Warn, Level::Info] {
            assert_eq!(
                Config {
                    level,
                    format: Format::Text,
                    programs: false,
                    ..Config::default()
                }
                .program_filter(),
                LevelFilter::OFF
            );
            assert_eq!(
                Config {
                    level,
                    format: Format::Text,
                    programs: true,
                    ..Config::default()
                }
                .program_filter(),
                LevelFilter::INFO
            );
        }
        for programs in [false, true] {
            assert_eq!(
                Config {
                    level: Level::Debug,
                    format: Format::Text,
                    programs,
                    ..Config::default()
                }
                .program_filter(),
                LevelFilter::DEBUG
            );
        }
    }

    #[test]
    fn output_normalization_uses_lowercase_levels() {
        assert_eq!(
            lowercase_json_level(r#"{"level":"INFO","count":2}"#),
            r#"{"level":"info","count":2}"#
        );
        assert_eq!(
            lowercase_text_level("2026-01-01T00:00:00Z  WARN message\n"),
            "2026-01-01T00:00:00Z  warn message\n"
        );
    }

    #[test]
    fn json_projection_preserves_types_and_required_fields() {
        let output = render(Format::Json);
        let lines = output.lines().collect::<Vec<_>>();
        assert_eq!(lines.len(), 1);
        let event: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(event["level"], "info");
        assert_eq!(event["event"], "test.event");
        assert_eq!(event["component"], "logging_test");
        assert_eq!(event["message"], "value with spaces and = signs");
        assert_eq!(event["count"], 7);
        assert_eq!(event["ready"], true);
        let timestamp = event["timestamp"].as_str().unwrap();
        let timestamp = chrono::DateTime::parse_from_rfc3339(timestamp).unwrap();
        assert_eq!(timestamp.offset().local_minus_utc(), 0);
    }

    #[test]
    fn text_and_logfmt_projections_are_one_event_per_line() {
        let text = render(Format::Text);
        assert_eq!(text.lines().count(), 1);
        assert!(text.contains(" info "));
        assert!(text.contains("event=\"test.event\""));
        assert!(text.contains("component=\"logging_test\""));

        let logfmt = render(Format::Logfmt);
        assert_eq!(logfmt.lines().count(), 1);
        assert!(logfmt.starts_with("timestamp=20"));
        assert!(logfmt.contains("level=info"));
        assert!(logfmt.contains("event=test.event"));
        assert!(logfmt.contains("message=\"value with spaces and = signs\""));
    }

    fn render(format: Format) -> String {
        let output = Arc::new(Mutex::new(Vec::new()));
        let writer = CaptureWriter(output.clone());
        match format {
            Format::Text => {
                let subscriber = tracing_subscriber::registry().with(
                    tracing_subscriber::fmt::layer()
                        .with_ansi(false)
                        .with_target(false)
                        .with_writer(writer)
                        .with_timer(UtcTime::rfc_3339()),
                );
                tracing::subscriber::with_default(subscriber, test_event);
            }
            Format::Json => {
                let subscriber = tracing_subscriber::registry().with(
                    tracing_subscriber::fmt::layer()
                        .json()
                        .flatten_event(true)
                        .with_current_span(false)
                        .with_span_list(false)
                        .with_target(false)
                        .with_writer(writer)
                        .with_timer(UtcTime::rfc_3339()),
                );
                tracing::subscriber::with_default(subscriber, test_event);
            }
            Format::Logfmt => {
                let subscriber = tracing_subscriber::registry().with(
                    tracing_logfmt::builder()
                        .with_target(false)
                        .with_span_name(false)
                        .with_span_path(false)
                        .layer()
                        .with_writer(writer),
                );
                tracing::subscriber::with_default(subscriber, test_event);
            }
        }
        let raw = String::from_utf8(output.lock().unwrap().clone()).unwrap();
        match format {
            Format::Text => lowercase_text_level(&raw),
            Format::Json => lowercase_json_level(&raw),
            Format::Logfmt => raw.replacen("ts=", "timestamp=", 1),
        }
    }

    fn test_event() {
        tracing::info!(
            event = "test.event",
            component = "logging_test",
            count = 7_u64,
            ready = true,
            message = "value with spaces and = signs"
        );
    }

    #[derive(Clone)]
    struct CaptureWriter(Arc<Mutex<Vec<u8>>>);

    impl<'a> MakeWriter<'a> for CaptureWriter {
        type Writer = Self;

        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    impl std::io::Write for CaptureWriter {
        fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buffer);
            Ok(buffer.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
}
