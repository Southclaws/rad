//! Loss-aware JSON encoding for engine result trees and PIR outcomes.

use serde::Serialize;
use serde_json::{Map, Number, Value as JsonValue};

use crate::engine::catalog::model::TransitionControl;
use crate::engine::exec::{ProgramResult, StatementPlan};
use crate::engine::lir::{Datum, ObjectField, Row, Value};
use crate::engine::planner::explain::PlanView;

#[derive(Debug, thiserror::Error)]
pub enum EncodeError {
    #[error("result contains a non-finite float")]
    NonFiniteFloat,
    #[error("result object contains duplicate field {0:?}")]
    DuplicateField(String),
    #[error("result metadata could not be encoded: {0}")]
    Metadata(#[from] serde_json::Error),
    #[error("statement affected count exceeds the HTTP wire format")]
    AffectedCountOverflow,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct EncodedProgramResult {
    pub result: JsonValue,
    pub statements: Vec<EncodedStatementResult>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plan: Option<PlanEnvelope>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct EncodedStatementResult {
    pub name: String,
    pub affected: usize,
    pub control: Option<TransitionControl>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct PlanEnvelope {
    pub statements: Vec<EncodedStatementPlan>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct EncodedStatementPlan {
    pub name: String,
    pub view: PlanView,
    pub text: String,
}

pub fn datum(value: &Datum) -> Result<JsonValue, EncodeError> {
    match value {
        Datum::Null => Ok(JsonValue::Null),
        Datum::Scalar(value) => scalar(value),
        Datum::Array(values) => values.iter().map(datum).collect(),
        Datum::Object(fields) => object(fields),
    }
}

pub fn row(value: &Row) -> Result<JsonValue, EncodeError> {
    let fields = value
        .iter()
        .map(|(name, value)| scalar(value).map(|value| (name.clone(), value)))
        .collect::<Result<Map<_, _>, _>>()?;
    Ok(JsonValue::Object(fields))
}

pub fn program(value: &ProgramResult) -> Result<EncodedProgramResult, EncodeError> {
    Ok(EncodedProgramResult {
        result: datum(&value.result)?,
        statements: value
            .statements
            .iter()
            .map(|statement| EncodedStatementResult {
                name: statement.name.clone(),
                affected: statement.affected,
                control: statement.control.clone(),
            })
            .collect(),
        plan: (!value.plans.is_empty()).then(|| plans(&value.plans)),
    })
}

fn plans(values: &[StatementPlan]) -> PlanEnvelope {
    PlanEnvelope {
        statements: values
            .iter()
            .map(|plan| EncodedStatementPlan {
                name: plan.name.clone(),
                view: plan.plan.clone(),
                text: plan.plan.render(),
            })
            .collect(),
    }
}

fn object(fields: &[ObjectField]) -> Result<JsonValue, EncodeError> {
    let mut output = Map::with_capacity(fields.len());
    for field in fields {
        if output.contains_key(&field.name) {
            return Err(EncodeError::DuplicateField(field.name.clone()));
        }
        output.insert(field.name.clone(), datum(&field.datum)?);
    }
    Ok(JsonValue::Object(output))
}

fn scalar(value: &Value) -> Result<JsonValue, EncodeError> {
    Ok(match value {
        Value::Text(value) => JsonValue::String(value.clone()),
        Value::Int64(value) => JsonValue::Number((*value).into()),
        Value::Float64(value) => {
            JsonValue::Number(Number::from_f64(*value).ok_or(EncodeError::NonFiniteFloat)?)
        }
        Value::Bool(value) => JsonValue::Bool(*value),
        Value::Null(_) => JsonValue::Null,
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::engine::catalog::model::ScalarType;
    use crate::engine::exec::StatementResult;

    #[test]
    fn encodes_nested_results_without_losing_int64_or_null_shape() {
        let value = Datum::Object(vec![
            ObjectField {
                name: "id".into(),
                datum: Datum::Scalar(Value::Int64(i64::MAX)),
            },
            ObjectField {
                name: "child".into(),
                datum: Datum::Null,
            },
            ObjectField {
                name: "items".into(),
                datum: Datum::Array(vec![Datum::Object(vec![ObjectField {
                    name: "name".into(),
                    datum: Datum::Scalar(Value::Text("rad".into())),
                }])]),
            },
        ]);

        assert_eq!(
            datum(&value).unwrap(),
            json!({
                "id": 9223372036854775807_i64,
                "child": null,
                "items": [{"name": "rad"}]
            })
        );
    }

    #[test]
    fn rejects_values_json_cannot_represent_losslessly() {
        assert!(matches!(
            datum(&Datum::Scalar(Value::Float64(f64::NAN))),
            Err(EncodeError::NonFiniteFloat)
        ));
        assert!(matches!(
            datum(&Datum::Object(vec![
                ObjectField {
                    name: "x".into(),
                    datum: Datum::Null,
                },
                ObjectField {
                    name: "x".into(),
                    datum: Datum::Scalar(Value::Null(ScalarType::Text)),
                },
            ])),
            Err(EncodeError::DuplicateField(field)) if field == "x"
        ));
    }

    #[test]
    fn program_results_always_emit_a_typed_control_slot() {
        let encoded = program(&ProgramResult {
            result: Datum::Null,
            statements: vec![StatementResult {
                name: "read".into(),
                affected: 0,
                control: None,
            }],
            plans: Vec::new(),
        })
        .unwrap();

        assert_eq!(
            serde_json::to_value(encoded).unwrap(),
            json!({
                "result": null,
                "statements": [{"name": "read", "affected": 0, "control": null}]
            })
        );
    }
}
