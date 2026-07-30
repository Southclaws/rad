use crate::engine::catalog::identity::CatalogVersion;
use crate::engine::catalog::model::{ReclamationState, RetentionHorizons, TransitionState};
use crate::engine::catalog::store;
use crate::engine::kv::{IsolationLevel, TransactionView};

use super::{Engine, Result};

#[derive(Clone, Debug, Default, PartialEq)]
pub struct SchemaStorageMetrics {
    pub retention_horizons: RetentionHorizons,
    pub canonical_catalog_revisions: u64,
    pub catalog_revision_compacted_through: CatalogVersion,
    pub active_transitions: u64,
    pub waiting_transitions: u64,
    pub failed_transitions: u64,
    pub uncompacted_terminal_transitions: u64,
    pub transition_delta_lag: u64,
    pub pending_reclamations: u64,
    pub failed_reclamations: u64,
    pub pinned_reclamations: u64,
    pub terminal_reclamation_records: u64,
    pub reclaimed_items: u64,
}

impl Engine {
    pub async fn schema_storage_metrics(&self) -> Result<SchemaStorageMetrics> {
        let transaction = self.store.begin(IsolationLevel::Snapshot).await?;
        let result = async {
            let mut view = TransactionView(&*transaction);
            let horizons = store::retention_horizons(&mut view).await?;
            let revisions = store::revisions(&mut view).await?;
            let compacted = store::revision_compacted_through(&mut view).await?;
            let mut transitions = store::list_transitions(&mut view).await?;
            let reclamations = store::list_reclamations(&mut view).await?;
            let mut metrics = SchemaStorageMetrics {
                retention_horizons: horizons,
                canonical_catalog_revisions: revisions.len() as u64,
                catalog_revision_compacted_through: compacted,
                ..SchemaStorageMetrics::default()
            };
            for transition in &mut transitions {
                let high_water = store::delta_high_water(&mut view, &transition.id).await?;
                transition.refresh_work_state(high_water);
                metrics.transition_delta_lag += transition
                    .delta_high_water
                    .saturating_sub(transition.applied_delta);
                match transition.state {
                    TransitionState::Waiting => {
                        metrics.active_transitions += 1;
                        metrics.waiting_transitions += 1;
                    }
                    TransitionState::Building
                    | TransitionState::CatchingUp
                    | TransitionState::Validating => metrics.active_transitions += 1,
                    TransitionState::Failed => metrics.failed_transitions += 1,
                    TransitionState::Ready | TransitionState::Cancelled => {}
                }
                if transition.state.is_terminal() && transition.compacted_at.is_zero() {
                    metrics.uncompacted_terminal_transitions += 1;
                }
            }
            for reclamation in reclamations {
                metrics.reclaimed_items += reclamation.items_reclaimed;
                match reclamation.state {
                    ReclamationState::Pending | ReclamationState::Reclaiming => {
                        metrics.pending_reclamations += 1;
                        if store::retention_blocker(&mut view, &reclamation)
                            .await?
                            .is_some()
                        {
                            metrics.pinned_reclamations += 1;
                        }
                    }
                    ReclamationState::Failed => metrics.failed_reclamations += 1,
                    ReclamationState::Reclaimed => metrics.terminal_reclamation_records += 1,
                }
            }
            Ok(metrics)
        }
        .await;
        transaction.rollback();
        result
    }
}
