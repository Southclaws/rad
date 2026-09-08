//! Direct JSON serialization for HTTP program results.
//!
//! The engine result can contain a large materialized relation. A generic
//! `serde_json::Value` tree would copy all object names, text values, arrays,
//! and objects before response serialization. These wrappers write the engine
//! datum directly and keep the public HTTP shape unchanged.

use serde::ser::{Error as _, SerializeMap as _, SerializeSeq as _, SerializeStruct as _};
use serde::{Serialize, Serializer};

use crate::engine::exec::{ProgramResult, StatementPlan, StatementResult};
use crate::engine::lir::{Datum, ObjectField, Value};
use crate::service::result_json::EncodeError;

pub(super) fn encode(value: &ProgramResult) -> Result<Vec<u8>, EncodeError> {
    for statement in &value.statements {
        i64::try_from(statement.affected).map_err(|_| EncodeError::AffectedCountOverflow)?;
    }
    serde_json::to_vec(&ProgramResultJson(value)).map_err(Into::into)
}

struct ProgramResultJson<'a>(&'a ProgramResult);

impl Serialize for ProgramResultJson<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut output = serializer
            .serialize_struct("ProgramResult", 2 + usize::from(!self.0.plans.is_empty()))?;
        if !self.0.plans.is_empty() {
            output.serialize_field("plan", &PlanEnvelopeJson(&self.0.plans))?;
        }
        output.serialize_field("result", &DatumJson(&self.0.result))?;
        output.serialize_field("statements", &StatementResultsJson(&self.0.statements))?;
        output.end()
    }
}

struct StatementResultsJson<'a>(&'a [StatementResult]);

impl Serialize for StatementResultsJson<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut output = serializer.serialize_seq(Some(self.0.len()))?;
        for statement in self.0 {
            output.serialize_element(&StatementResultJson(statement))?;
        }
        output.end()
    }
}

struct StatementResultJson<'a>(&'a StatementResult);

impl Serialize for StatementResultJson<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let affected = i64::try_from(self.0.affected).map_err(S::Error::custom)?;
        let mut output = serializer.serialize_struct("StatementResult", 3)?;
        output.serialize_field("affected", &affected)?;
        output.serialize_field("control", &self.0.control)?;
        output.serialize_field("name", &self.0.name)?;
        output.end()
    }
}

struct PlanEnvelopeJson<'a>(&'a [StatementPlan]);

impl Serialize for PlanEnvelopeJson<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut output = serializer.serialize_struct("PlanEnvelope", 1)?;
        output.serialize_field("statements", &StatementPlansJson(self.0))?;
        output.end()
    }
}

struct StatementPlansJson<'a>(&'a [StatementPlan]);

impl Serialize for StatementPlansJson<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut output = serializer.serialize_seq(Some(self.0.len()))?;
        for plan in self.0 {
            output.serialize_element(&StatementPlanJson(plan))?;
        }
        output.end()
    }
}

struct StatementPlanJson<'a>(&'a StatementPlan);

impl Serialize for StatementPlanJson<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut output = serializer.serialize_struct(
            "StatementPlan",
            3 + usize::from(self.0.measurement.is_some()),
        )?;
        output.serialize_field("name", &self.0.name)?;
        output.serialize_field("view", &self.0.plan)?;
        output.serialize_field("text", &self.0.plan.render())?;
        if let Some(measurement) = &self.0.measurement {
            output.serialize_field("measurement", measurement)?;
        }
        output.end()
    }
}

struct DatumJson<'a>(&'a Datum);

impl Serialize for DatumJson<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self.0 {
            Datum::Null => serializer.serialize_none(),
            Datum::Scalar(value) => ScalarJson(value).serialize(serializer),
            Datum::Array(values) => {
                let mut output = serializer.serialize_seq(Some(values.len()))?;
                for value in values {
                    output.serialize_element(&DatumJson(value))?;
                }
                output.end()
            }
            Datum::Object(fields) => serialize_object(fields, serializer),
        }
    }
}

fn serialize_object<S>(fields: &[ObjectField], serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    // JSON permits repeated object names in its text grammar, but Rad result
    // objects do not. The inline name set avoids a heap allocation for the
    // common narrow-row case. A duplicate must fail before its value enters
    // the response body.
    let mut names = smallvec::SmallVec::<[&str; 8]>::new();
    let mut output = serializer.serialize_map(Some(fields.len()))?;
    for field in fields {
        if names.contains(&field.name.as_str()) {
            return Err(S::Error::custom(format_args!(
                "result object contains duplicate field {:?}",
                field.name
            )));
        }
        names.push(&field.name);
        output.serialize_entry(&field.name, &DatumJson(&field.datum))?;
    }
    output.end()
}

struct ScalarJson<'a>(&'a Value);

impl Serialize for ScalarJson<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self.0 {
            Value::Text(value) => serializer.serialize_str(value),
            Value::Int64(value) => serializer.serialize_i64(*value),
            Value::Float64(value) if value.is_finite() => serializer.serialize_f64(*value),
            Value::Float64(_) => Err(S::Error::custom("result contains a non-finite float")),
            Value::Bool(value) => serializer.serialize_bool(*value),
            Value::Null(_) => serializer.serialize_none(),
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::engine::exec::StatementResult;

    #[test]
    fn encodes_the_program_wire_shape_without_an_intermediate_json_tree() {
        let value = ProgramResult {
            result: Datum::Object(vec![ObjectField {
                name: "items".into(),
                datum: Datum::Array(vec![Datum::Object(vec![ObjectField {
                    name: "value".into(),
                    datum: Datum::Scalar(Value::Int64(i64::MAX)),
                }])]),
            }]),
            statements: vec![StatementResult {
                name: "read".into(),
                affected: 1,
                control: None,
            }],
            plans: Vec::new(),
        };

        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&encode(&value).unwrap()).unwrap(),
            json!({
                "result": {"items": [{"value": i64::MAX}]},
                "statements": [{"name": "read", "affected": 1, "control": null}]
            })
        );
    }

    #[test]
    fn rejects_result_values_that_json_cannot_represent() {
        let result = |result| ProgramResult {
            result,
            statements: Vec::new(),
            plans: Vec::new(),
        };

        assert!(encode(&result(Datum::Scalar(Value::Float64(f64::NAN)))).is_err());
        assert!(
            encode(&result(Datum::Object(vec![
                ObjectField {
                    name: "x".into(),
                    datum: Datum::Null,
                },
                ObjectField {
                    name: "x".into(),
                    datum: Datum::Null,
                },
            ])))
            .is_err()
        );
    }
}
