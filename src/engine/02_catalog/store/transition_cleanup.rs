use crate::engine::catalog::Result;
use crate::engine::catalog::model::{
    ReclamationState, SchemaTransition, TransitionKind, TransitionState,
};
use crate::engine::kv::KvView;

use super::{
    cancelled_index_reclamation_id, cancelled_replacement_reclamation_id,
    constraint_validation_reclamation_id, failed_index_reclamation_id,
    failed_replacement_reclamation_id, get_reclamation, replaced_column_reclamation_id,
    transition_delta_reclamation_id, transition_diagnostics_pinned,
};

pub async fn transition_compaction_eligible<V: KvView + ?Sized>(
    view: &mut V,
    transition: &SchemaTransition,
) -> Result<bool> {
    if !transition.compacted_at.is_zero() {
        return Ok(false);
    }
    let terminal = transition.state.is_terminal();
    let legacy_without_cleanup = (transition.kind == TransitionKind::IndexBuild
        && transition.index_request.is_some()
        && transition.index.column_ids.is_empty())
        || (transition.kind == TransitionKind::ColumnReplacement
            && transition.column_replacement.is_none());
    if terminal && legacy_without_cleanup {
        return Ok(!transition_diagnostics_pinned(view, &transition.id).await?);
    }

    let reclamation_id = match (transition.state, transition.kind) {
        (TransitionState::Ready, TransitionKind::IndexBuild) => {
            transition_delta_reclamation_id(&transition.id)
        }
        (TransitionState::Ready, TransitionKind::ColumnReplacement) => {
            replaced_column_reclamation_id(&transition.id)
        }
        (TransitionState::Ready, TransitionKind::ConstraintValidation)
        | (TransitionState::Cancelled, TransitionKind::ConstraintValidation)
        | (TransitionState::Failed, TransitionKind::ConstraintValidation) => {
            constraint_validation_reclamation_id(&transition.id)
        }
        (TransitionState::Cancelled, TransitionKind::IndexBuild) => {
            cancelled_index_reclamation_id(&transition.id)
        }
        (TransitionState::Cancelled, TransitionKind::ColumnReplacement) => {
            cancelled_replacement_reclamation_id(&transition.id)
        }
        (TransitionState::Failed, TransitionKind::IndexBuild) => {
            failed_index_reclamation_id(&transition.id)
        }
        (TransitionState::Failed, TransitionKind::ColumnReplacement) => {
            failed_replacement_reclamation_id(&transition.id)
        }
        _ => return Ok(false),
    };
    if transition_diagnostics_pinned(view, &transition.id).await? {
        return Ok(false);
    }
    Ok(get_reclamation(view, &reclamation_id)
        .await?
        .is_some_and(|reclamation| reclamation.state == ReclamationState::Reclaimed))
}
