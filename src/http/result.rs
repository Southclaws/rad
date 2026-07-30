use super::generated::types as wire;
use crate::engine::exec::ProgramResult;
use crate::service::result_json::{self, EncodeError};

pub(super) fn encode(value: &ProgramResult) -> Result<wire::ProgramResult, EncodeError> {
    let encoded = result_json::program(value)?;
    let statements = encoded
        .statements
        .into_iter()
        .map(|statement| {
            Ok(wire::StatementResult {
                affected: i64::try_from(statement.affected)
                    .map_err(|_| EncodeError::AffectedCountOverflow)?,
                control: serde_json::to_value(statement.control)?,
                name: statement.name,
            })
        })
        .collect::<Result<_, EncodeError>>()?;

    Ok(wire::ProgramResult {
        plan: encoded
            .plan
            .map(serde_json::to_value)
            .transpose()
            .map_err(EncodeError::Metadata)?,
        result: encoded.result,
        statements,
    })
}
