use axum::http::StatusCode;

use super::generated::server::{
    AdministrationApi, SchemaTransitionCancelResponse, SchemaTransitionGetResponse,
    SchemaTransitionListResponse,
};
use super::generated::types::{TransitionKind, TransitionList, TransitionState};
use super::problem::ResponseProblem;
use super::server::Api;
use super::{server, wire};
use crate::engine::catalog::identity::TransitionId;
use crate::engine::exec::{Error, ErrorReason};
use crate::engine::frontend::schema_transitions::{
    cancel_schema_transition, schema_transition, schema_transitions,
};
use crate::service::error::{Failure, NotFoundReason, ResourceContext, Stage};

#[async_trait::async_trait]
impl AdministrationApi for Api {
    async fn schema_transition_list(
        &self,
        kind: Option<TransitionKind>,
        state: Option<TransitionState>,
    ) -> SchemaTransitionListResponse {
        let controls = match schema_transitions(&self.engine).await {
            Ok(controls) => controls,
            Err(error) => return list_problem(server::engine_problem(&error)),
        };
        let mut transitions = Vec::with_capacity(controls.len());
        for control in controls {
            let value = match wire::transition(&control) {
                Ok(value) => value,
                Err(error) => {
                    return list_problem(server::internal_problem(
                        "encode schema transition",
                        error,
                    ));
                }
            };
            if kind
                .as_ref()
                .is_some_and(|kind| kind != &value.transition_kind)
                || state.as_ref().is_some_and(|state| state != &value.state)
            {
                continue;
            }
            transitions.push(value);
        }
        SchemaTransitionListResponse::Ok(TransitionList { transitions })
    }

    async fn schema_transition_get(&self, transition: String) -> SchemaTransitionGetResponse {
        let id = TransitionId::from(transition.as_str());
        let control = match schema_transition(&self.engine, &id).await {
            Ok(control) => control,
            Err(error) => return get_problem(transition_problem(&error, &transition)),
        };
        match wire::transition(&control) {
            Ok(control) => SchemaTransitionGetResponse::Ok(control),
            Err(error) => get_problem(server::internal_problem("encode schema transition", error)),
        }
    }

    async fn schema_transition_cancel(&self, transition: String) -> SchemaTransitionCancelResponse {
        let id = TransitionId::from(transition.as_str());
        let control = match cancel_schema_transition(&self.engine, &id).await {
            Ok(control) => control,
            Err(error) => return cancel_problem(transition_problem(&error, &transition)),
        };
        match wire::transition(&control) {
            Ok(control) => SchemaTransitionCancelResponse::Ok(control),
            Err(error) => {
                cancel_problem(server::internal_problem("encode schema transition", error))
            }
        }
    }
}

fn transition_problem(error: &Error, transition: &str) -> ResponseProblem {
    if error.reason() == ErrorReason::SchemaTransitionNotFound {
        return ResponseProblem::from_failure(Failure::not_found(
            Stage::Preflight,
            NotFoundReason::SchemaTransition,
            error.to_string(),
            Some(ResourceContext {
                kind: "schema_transition".into(),
                name: Some(transition.into()),
            }),
        ));
    }
    server::engine_problem(error)
}

fn list_problem(problem: ResponseProblem) -> SchemaTransitionListResponse {
    SchemaTransitionListResponse::Default(problem.status, problem.body)
}

fn get_problem(problem: ResponseProblem) -> SchemaTransitionGetResponse {
    match problem.status {
        StatusCode::NOT_FOUND => SchemaTransitionGetResponse::NotFound(problem.body),
        status => SchemaTransitionGetResponse::Default(status, problem.body),
    }
}

fn cancel_problem(problem: ResponseProblem) -> SchemaTransitionCancelResponse {
    match problem.status {
        StatusCode::NOT_FOUND => SchemaTransitionCancelResponse::NotFound(problem.body),
        StatusCode::CONFLICT => SchemaTransitionCancelResponse::Conflict(problem.body),
        StatusCode::UNPROCESSABLE_ENTITY => {
            SchemaTransitionCancelResponse::UnprocessableEntity(problem.body)
        }
        status => SchemaTransitionCancelResponse::Default(status, problem.body),
    }
}
