//! Transport-neutral administrative schema-transition controls.

use crate::engine::catalog::identity::TransitionId;
use crate::engine::catalog::model::TransitionControl;
use crate::engine::exec::{Engine, Result};

pub async fn schema_transition(engine: &Engine, id: &TransitionId) -> Result<TransitionControl> {
    Ok(engine.inspect_schema_transition(id).await?.control())
}

pub async fn schema_transitions(engine: &Engine) -> Result<Vec<TransitionControl>> {
    Ok(engine
        .list_schema_transitions()
        .await?
        .into_iter()
        .map(|transition| transition.control())
        .collect())
}

/// Cancel durable work without advancing its worker. Transport adapters may
/// expose this as an administrative operation, never as an interleaved PIR
/// statement.
pub async fn cancel_schema_transition(
    engine: &Engine,
    id: &TransitionId,
) -> Result<TransitionControl> {
    Ok(engine.cancel_schema_transition(id).await?.control())
}
