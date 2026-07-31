use std::error::Error;

use serde::Serialize;
use serde_json::{Value, json};

use super::client::ApiError;
use super::generated::{GlobalArgs, GlobalOutput};
use crate::process::Result;

pub(super) fn is_json(globals: &GlobalArgs) -> bool {
    globals.json || globals.output == GlobalOutput::Json
}

pub(super) fn json_value(value: &impl Serialize) -> Result<Value> {
    Ok(serde_json::to_value(value)?)
}

pub(super) fn print_json(value: &impl Serialize) -> Result {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

pub(super) fn progress(globals: &GlobalArgs, message: impl AsRef<str>) {
    if is_json(globals) {
        eprintln!("{}", message.as_ref());
    } else {
        println!("{}", message.as_ref());
    }
}

#[derive(Debug)]
pub(super) struct CliError {
    code: &'static str,
    message: String,
    details: Option<Value>,
    text: Option<String>,
}

impl CliError {
    pub(super) fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            details: None,
            text: None,
        }
    }

    pub(super) fn with_details(
        code: &'static str,
        message: impl Into<String>,
        details: Value,
    ) -> Self {
        Self {
            code,
            message: message.into(),
            details: Some(details),
            text: None,
        }
    }

    pub(super) fn with_text(mut self, text: impl Into<String>) -> Self {
        self.text = Some(text.into());
        self
    }

    fn value(&self) -> Value {
        error_value(self.code, &self.message, self.details.clone())
    }
}

impl std::fmt::Display for CliError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.text.as_deref().unwrap_or(&self.message))
    }
}

impl Error for CliError {}

pub(super) fn render_error(error: &(dyn Error + 'static), json_output: bool) {
    if json_output {
        let value = if let Some(error) = error.downcast_ref::<CliError>() {
            error.value()
        } else if let Some(error) = error.downcast_ref::<ApiError>() {
            error.value()
        } else {
            error_value("error", &error.to_string(), None)
        };
        eprintln!(
            "{}",
            serde_json::to_string_pretty(&value).expect("JSON error values serialize")
        );
    } else {
        eprintln!("error: {error}");
    }
}

fn error_value(code: &str, message: &str, details: Option<Value>) -> Value {
    let mut error = serde_json::Map::from_iter([
        ("code".into(), Value::String(code.into())),
        ("message".into(), Value::String(message.into())),
    ]);
    if let Some(details) = details {
        error.insert("details".into(), details);
    }
    json!({ "ok": false, "error": error })
}
