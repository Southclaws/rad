//! Declarative schema migration orchestration.
//!
//! Planning and apply are transport-neutral. Durable work is returned as a
//! converging result and observed with [`Migration::refresh`]; callers retain
//! control of polling and schema-job scheduling.

mod plan;
mod preflight;
mod program;

use crate::engine::catalog::identity::TransitionId;
use crate::engine::catalog::migrate::Step;
use crate::engine::catalog::model::{Revision, Schema, TransitionControl, TransitionState};
use crate::engine::catalog::schema;
use crate::engine::exec::{
    CatalogExpectation, CatalogPolicy, Engine, Error, ErrorKind, Program, ProgramOptions, Result,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SchemaFinding {
    pub kind: String,
    pub summary: String,
    pub table: String,
    pub column: String,
    pub rows: u64,
}

impl SchemaFinding {
    fn new(kind: &str, summary: impl Into<String>) -> Self {
        Self {
            kind: kind.into(),
            summary: summary.into(),
            table: String::new(),
            column: String::new(),
            rows: 0,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct MigrationPlan {
    pub current: Revision,
    pub desired: Schema,
    pub desired_hash: String,
    pub steps: Vec<Step>,
    pub program: Program,
    pub transitions: Vec<TransitionControl>,
    pub destructive: Vec<SchemaFinding>,
    pub blocking: Vec<SchemaFinding>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MigrationState {
    Converging,
    Ready,
}

#[derive(Clone, Debug, PartialEq)]
pub struct MigrationResult {
    pub plan: MigrationPlan,
    pub revision: Revision,
    pub state: MigrationState,
    pub transition_ids: Vec<TransitionId>,
}

pub struct Migration<'a> {
    engine: &'a Engine,
}

impl<'a> Migration<'a> {
    pub fn new(engine: &'a Engine) -> Self {
        Self { engine }
    }

    pub async fn plan(&self, desired: &schema::Schema) -> Result<MigrationPlan> {
        plan::build(self.engine, desired).await
    }

    pub async fn plan_file(&self, filename: &str, source: &[u8]) -> Result<MigrationPlan> {
        let desired = schema::parse(filename, source)?;
        self.plan(&desired).await
    }

    pub async fn apply(
        &self,
        desired: &schema::Schema,
        accept_data_loss: bool,
    ) -> Result<MigrationResult> {
        let plan = self.plan(desired).await?;
        self.apply_plan(plan, accept_data_loss).await
    }

    pub async fn apply_file(
        &self,
        filename: &str,
        source: &[u8],
        accept_data_loss: bool,
    ) -> Result<MigrationResult> {
        let plan = self.plan_file(filename, source).await?;
        self.apply_plan(plan, accept_data_loss).await
    }

    pub async fn apply_plan(
        &self,
        plan: MigrationPlan,
        accept_data_loss: bool,
    ) -> Result<MigrationResult> {
        if let Some(finding) = plan.blocking.first() {
            return Err(Error::message(
                ErrorKind::ConstraintViolation,
                format!("migration target is invalid: {}", finding.summary),
            ));
        }
        if !accept_data_loss && let Some(finding) = plan.destructive.first() {
            return Err(Error::message(
                ErrorKind::DataLossAcceptance,
                format!("migration will delete data: {}", finding.summary),
            ));
        }

        let mut controls = plan.transitions.clone();
        if !plan.program.statements.is_empty() {
            let executed = self
                .engine
                .execute_program_with_options(
                    plan.program.clone(),
                    ProgramOptions {
                        catalog: CatalogPolicy::RevisionPerProgram,
                        expected_catalog: Some(CatalogExpectation::from(&plan.current)),
                        ..ProgramOptions::default()
                    },
                )
                .await?;
            for statement in executed.statements {
                if let Some(control) = statement.control {
                    controls.push(control);
                }
            }
        }

        let (revision, _, _) = self.engine.schema_migration_snapshot().await?;
        if plan.program.statements.is_empty()
            && (revision.version != plan.current.version || revision.hash != plan.current.hash)
            && revision.hash != plan.desired_hash
        {
            return Err(Error::message(
                ErrorKind::Conflict,
                "migration: catalog changed after planning",
            ));
        }
        let mut transition_ids = controls
            .into_iter()
            .map(|control| control.transition_id)
            .collect::<Vec<_>>();
        transition_ids.sort_by(|left, right| left.as_str().cmp(right.as_str()));
        transition_ids.dedup();
        let state = if revision.hash == plan.desired_hash {
            MigrationState::Ready
        } else if transition_ids.is_empty() {
            return Err(Error::message(
                ErrorKind::Internal,
                format!(
                    "migration: catalog reached hash {} without durable work toward {}",
                    revision.hash, plan.desired_hash
                ),
            ));
        } else {
            MigrationState::Converging
        };
        Ok(MigrationResult {
            plan,
            revision,
            state,
            transition_ids,
        })
    }

    /// Observe convergence once. This method never sleeps or advances work.
    pub async fn refresh(&self, mut result: MigrationResult) -> Result<MigrationResult> {
        if result.state == MigrationState::Ready {
            return Ok(result);
        }
        if result.transition_ids.is_empty() {
            return Err(Error::message(
                ErrorKind::Internal,
                "migration: converging result has no durable transition IDs",
            ));
        }
        let mut all_ready = true;
        for id in &result.transition_ids {
            let transition = self.engine.inspect_schema_transition(id).await?;
            match transition.state {
                TransitionState::Ready => {}
                TransitionState::Failed | TransitionState::Cancelled => {
                    return Err(Error::message(
                        ErrorKind::ConstraintViolation,
                        format!(
                            "migration: transition {id:?} ended in state {:?}: {}",
                            transition.state, transition.last_error
                        ),
                    ));
                }
                _ => all_ready = false,
            }
        }
        if !all_ready {
            return Ok(result);
        }
        let (revision, _, _) = self.engine.schema_migration_snapshot().await?;
        if revision.hash != result.plan.desired_hash {
            return Err(Error::message(
                ErrorKind::Conflict,
                format!(
                    "migration: transitions published but current hash is {}, want {}",
                    revision.hash, result.plan.desired_hash
                ),
            ));
        }
        result.revision = revision;
        result.state = MigrationState::Ready;
        Ok(result)
    }
}

#[cfg(test)]
mod tests;
