use crate::engine::catalog::identity::{AccessGeneration, OwnerEpoch, TableId};
use crate::engine::catalog::model::{
    DataPosition, Index, Timestamp, TransitionState, TransitionWorkState,
};
use crate::engine::catalog::{Error, ErrorKind, Result};
use crate::engine::kv::KvView;

use super::{get_transition, save_transition};
use crate::engine::catalog::identity::TransitionId;

pub async fn compact_transition<V: KvView + ?Sized>(
    view: &mut V,
    id: &TransitionId,
    completed_at: Timestamp,
) -> Result<()> {
    let Some(mut transition) = get_transition(view, id).await? else {
        return Err(Error::message(
            ErrorKind::NotFound,
            format!("catalog: transition {id:?} does not exist"),
        ));
    };
    if !matches!(
        transition.state,
        TransitionState::Ready | TransitionState::Cancelled | TransitionState::Failed
    ) {
        return Err(Error::message(
            ErrorKind::InvalidInput,
            format!("catalog: transition {id:?} is not terminal"),
        ));
    }
    if !transition.compacted_at.is_zero() {
        return Ok(());
    }
    transition.index = Index {
        id: transition.index.id,
        logical_id: transition.index.logical_id,
        definition_generation: transition.index.definition_generation,
        access_generation: AccessGeneration::ZERO,
        state: transition.index.state,
        name: transition.index.name,
        columns: Vec::new(),
        column_ids: Vec::new(),
        unique: transition.index.unique,
    };
    transition.owner_epoch = OwnerEpoch::ZERO;
    transition.base_position = DataPosition::default();
    transition.barrier_position = DataPosition::default();
    transition.table_id = TableId::default();
    transition.cursor.clear();
    transition.batch_id = 0;
    transition.applied_delta = 0;
    transition.delta_high_water = 0;
    transition.delta_soft_limit = 0;
    transition.delta_hard_limit = 0;
    transition.work_state = TransitionWorkState::Normal;
    transition.rows_scanned = 0;
    transition.affected_column_ids.clear();
    transition.index_request = None;
    transition.column_replacement = None;
    transition.replacement_request = None;
    transition.constraint = None;
    transition.constraint_request = None;
    transition.prerequisites.clear();
    transition.gate_table_ids.clear();
    transition.compacted_at = completed_at;
    save_transition(view, &transition).await
}
