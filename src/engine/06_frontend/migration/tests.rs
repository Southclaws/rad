use std::sync::Arc;

use crate::engine::catalog::model::{ScalarType, TransitionKind, TransitionState};
use crate::engine::catalog::schema;
use crate::engine::exec::{Engine, ErrorKind, Statement};
use crate::engine::kv::slatedb::Store;
use crate::engine::lir::{Row, Value};

use super::{Migration, MigrationState};

fn parse(source: &str) -> schema::Schema {
    schema::parse("test.schema.yaml", source.as_bytes()).unwrap()
}

async fn finish_transitions(
    engine: &Engine,
    ids: &[crate::engine::catalog::identity::TransitionId],
) {
    for _ in 0..16 {
        let mut terminal = 0;
        for id in ids {
            let mut transition = engine.inspect_schema_transition(id).await.unwrap();
            if transition.state.is_terminal() {
                terminal += 1;
                continue;
            }
            if transition.state == TransitionState::Waiting {
                transition = engine.activate_waiting_schema_transition(id).await.unwrap();
                if transition.state == TransitionState::Waiting {
                    continue;
                }
            }
            let owner = engine.claim_schema_transition(id).await.unwrap();
            for _ in 0..32 {
                let step = engine.step_schema_transition(id, owner, 1).await.unwrap();
                if step.transition.state.is_terminal() {
                    break;
                }
            }
        }
        if terminal == ids.len() {
            return;
        }
    }
    panic!("migration transitions did not converge");
}

#[tokio::test]
async fn apply_returns_convergence_and_replanning_recovers_exact_active_work() {
    let store = Arc::new(Store::memory("frontend-migration-online").await.unwrap());
    let engine = Engine::new(store);
    let migration = Migration::new(&engine);
    let initial = parse(
        r#"
tables:
  - id: 1
    name: items
    columns:
      - { id: 1, name: id, type: string, pk: true }
      - { id: 2, name: value, type: string }
"#,
    );
    let created = migration.apply(&initial, false).await.unwrap();
    assert_eq!(created.state, MigrationState::Ready);
    assert_eq!(created.revision.version.get(), 1);
    for (id, value) in [("a", "41"), ("b", "42")] {
        engine
            .create(
                "items",
                Row::from([
                    ("id".into(), Value::Text(id.into())),
                    ("value".into(), Value::Text(value.into())),
                ]),
            )
            .await
            .unwrap();
    }
    let desired = parse(
        r#"
tables:
  - id: 1
    name: items
    columns:
      - { id: 1, name: id, type: string, pk: true }
      - { id: 2, name: value, type: int64, index: true }
"#,
    );
    let plan = migration.plan(&desired).await.unwrap();
    assert_eq!(plan.program.statements.len(), 2);
    assert!(matches!(
        &plan.program.statements[0],
        Statement::StartColumnReplacement { .. }
    ));
    assert!(matches!(
        &plan.program.statements[1],
        Statement::StartIndexBuild { after, .. } if after == &["migration_1"]
    ));
    let result = migration.apply_plan(plan, false).await.unwrap();
    assert_eq!(result.state, MigrationState::Converging);
    assert_eq!(result.transition_ids.len(), 2);
    assert_eq!(result.revision.version, created.revision.version);

    let recovered = migration.plan(&desired).await.unwrap();
    assert!(recovered.program.statements.is_empty());
    assert_eq!(recovered.transitions.len(), 2);

    finish_transitions(&engine, &result.transition_ids).await;
    let ready = migration.refresh(result).await.unwrap();
    assert_eq!(ready.state, MigrationState::Ready);
    assert_eq!(ready.revision.hash, ready.plan.desired_hash);
    // The replacement and its dependent index become independently visible
    // canonical states. Internal transition setup consumed no version; each
    // actual canonical publication consumed one.
    assert_eq!(ready.revision.version.get(), 3);
    let (_, tables, _) = engine.schema_migration_snapshot().await.unwrap();
    let value = tables[0]
        .columns
        .iter()
        .find(|column| column.name == "value")
        .unwrap();
    assert_eq!(value.scalar_type, ScalarType::Int64);
    assert!(tables[0].indexes.iter().any(|index| index.is_ready()));
}

#[tokio::test]
async fn data_findings_block_invalid_targets_and_require_deletion_consent() {
    let store = Arc::new(Store::memory("frontend-migration-findings").await.unwrap());
    let engine = Engine::new(store);
    let migration = Migration::new(&engine);
    let initial = parse(
        r#"
tables:
  - id: 1
    name: items
    columns:
      - { id: 1, name: id, type: string, pk: true }
      - { id: 2, name: label, type: string, nullable: true }
"#,
    );
    migration.apply(&initial, false).await.unwrap();
    engine
        .create(
            "items",
            Row::from([
                ("id".into(), Value::Text("a".into())),
                ("label".into(), Value::Null(ScalarType::Text)),
            ]),
        )
        .await
        .unwrap();
    for id in ["b", "c"] {
        engine
            .create(
                "items",
                Row::from([
                    ("id".into(), Value::Text(id.into())),
                    ("label".into(), Value::Text("duplicate".into())),
                ]),
            )
            .await
            .unwrap();
    }
    let not_null = parse(
        r#"
tables:
  - id: 1
    name: items
    columns:
      - { id: 1, name: id, type: string, pk: true }
      - { id: 2, name: label, type: string }
"#,
    );
    let before_blocked = engine.schema_migration_snapshot().await.unwrap();
    let blocked = migration.plan(&not_null).await.unwrap();
    assert_eq!(blocked.blocking[0].kind, "not_null_existing_nulls");
    assert_eq!(
        migration
            .apply_plan(blocked, false)
            .await
            .unwrap_err()
            .kind(),
        ErrorKind::ConstraintViolation
    );
    assert_eq!(
        engine.schema_migration_snapshot().await.unwrap(),
        before_blocked
    );

    let unique = parse(
        r#"
tables:
  - id: 1
    name: items
    columns:
      - { id: 1, name: id, type: string, pk: true }
      - { id: 2, name: label, type: string, nullable: true, unique: true }
"#,
    );
    let duplicate = migration.plan(&unique).await.unwrap();
    assert_eq!(duplicate.blocking[0].kind, "unique_index_duplicates");
    assert_eq!(duplicate.blocking[0].rows, 1);
    assert_eq!(
        migration
            .apply_plan(duplicate, false)
            .await
            .unwrap_err()
            .kind(),
        ErrorKind::ConstraintViolation
    );
    assert_eq!(
        engine.schema_migration_snapshot().await.unwrap(),
        before_blocked
    );

    let converted = parse(
        r#"
tables:
  - id: 1
    name: items
    columns:
      - { id: 1, name: id, type: string, pk: true }
      - { id: 2, name: label, type: int64, nullable: true }
"#,
    );
    let conversion = migration.plan(&converted).await.unwrap();
    assert_eq!(conversion.blocking[0].kind, "column_conversion");
    assert_eq!(conversion.blocking[0].rows, 2);
    assert_eq!(
        migration
            .apply_plan(conversion, false)
            .await
            .unwrap_err()
            .kind(),
        ErrorKind::ConstraintViolation
    );
    assert_eq!(
        engine.schema_migration_snapshot().await.unwrap(),
        before_blocked
    );
    assert!(engine.list_schema_transitions().await.unwrap().is_empty());

    let empty = schema::Schema { tables: Vec::new() };
    let destructive = migration.plan(&empty).await.unwrap();
    assert_eq!(destructive.destructive[0].kind, "delete_table");
    assert_eq!(destructive.destructive[0].rows, 3);
    assert_eq!(
        migration
            .apply_plan(destructive.clone(), false)
            .await
            .unwrap_err()
            .kind(),
        ErrorKind::DataLossAcceptance
    );
    assert_eq!(
        migration.apply_plan(destructive, true).await.unwrap().state,
        MigrationState::Ready
    );
}

#[tokio::test]
async fn preflight_detects_collisions_created_by_conversion_and_historical_missing_values() {
    let store = Arc::new(
        Store::memory("frontend-migration-derived-collisions")
            .await
            .unwrap(),
    );
    let engine = Engine::new(store);
    let migration = Migration::new(&engine);
    let initial = parse(
        r#"
tables:
  - id: 1
    name: converted
    columns:
      - { id: 1, name: id, type: int64, pk: true }
      - { id: 2, name: value, type: string }
  - id: 2
    name: historical
    columns:
      - { id: 1, name: id, type: int64, pk: true }
"#,
    );
    migration.apply(&initial, false).await.unwrap();
    for (id, value) in [(1, "1"), (2, "01")] {
        engine
            .create(
                "converted",
                Row::from([
                    ("id".into(), Value::Int64(id)),
                    ("value".into(), Value::Text(value.into())),
                ]),
            )
            .await
            .unwrap();
        engine
            .create("historical", Row::from([("id".into(), Value::Int64(id))]))
            .await
            .unwrap();
    }
    let desired = parse(
        r#"
tables:
  - id: 1
    name: converted
    columns:
      - { id: 1, name: id, type: int64, pk: true }
      - { id: 2, name: value, type: int64, unique: true }
  - id: 2
    name: historical
    columns:
      - { id: 1, name: id, type: int64, pk: true }
      - { id: 2, name: code, type: string, default: same, unique: true }
"#,
    );
    let before = engine.schema_migration_snapshot().await.unwrap();
    let plan = migration.plan(&desired).await.unwrap();
    let mut findings = plan
        .blocking
        .iter()
        .map(|finding| (finding.kind.as_str(), finding.rows))
        .collect::<Vec<_>>();
    findings.sort_unstable();
    assert_eq!(
        findings,
        [
            ("unique_index_duplicates", 1),
            ("unique_index_duplicates", 1)
        ]
    );
    assert_eq!(
        migration.apply_plan(plan, false).await.unwrap_err().kind(),
        ErrorKind::ConstraintViolation
    );
    assert_eq!(engine.schema_migration_snapshot().await.unwrap(), before);
    assert!(engine.list_schema_transitions().await.unwrap().is_empty());
}

#[tokio::test]
async fn exact_plan_is_rejected_after_catalog_changes() {
    let store = Arc::new(Store::memory("frontend-migration-fence").await.unwrap());
    let engine = Engine::new(store);
    let migration = Migration::new(&engine);
    let first = parse(
        r#"
tables:
  - id: 1
    name: first
    columns:
      - { id: 1, name: id, type: string, pk: true }
"#,
    );
    let stale = migration.plan(&first).await.unwrap();
    let other = parse(
        r#"
tables:
  - id: 2
    name: other
    columns:
      - { id: 1, name: id, type: string, pk: true }
"#,
    );
    migration.apply(&other, false).await.unwrap();
    assert_eq!(
        migration.apply_plan(stale, false).await.unwrap_err().kind(),
        ErrorKind::Conflict
    );
}

#[tokio::test]
async fn competing_migration_plans_have_one_serializable_winner() {
    let store = Arc::new(Store::memory("frontend-migration-race").await.unwrap());
    let engine = Engine::new(store);
    let migration = Migration::new(&engine);
    let initial = parse(
        r#"
tables:
  - id: 1
    name: items
    columns:
      - { id: 1, name: id, type: string, pk: true }
      - { id: 2, name: value, type: string }
"#,
    );
    migration.apply(&initial, false).await.unwrap();
    let as_integer = parse(
        r#"
tables:
  - id: 1
    name: items
    columns:
      - { id: 1, name: id, type: string, pk: true }
      - { id: 2, name: value, type: int64 }
"#,
    );
    let as_boolean = parse(
        r#"
tables:
  - id: 1
    name: items
    columns:
      - { id: 1, name: id, type: string, pk: true }
      - { id: 2, name: value, type: bool }
"#,
    );
    let integer_plan = migration.plan(&as_integer).await.unwrap();
    let boolean_plan = migration.plan(&as_boolean).await.unwrap();
    assert_eq!(integer_plan.current, boolean_plan.current);

    let (integer_result, boolean_result) = tokio::join!(
        migration.apply_plan(integer_plan, false),
        migration.apply_plan(boolean_plan, false),
    );
    let (winner, winner_schema, loser_error, loser_schema) = match (integer_result, boolean_result)
    {
        (Ok(winner), Err(loser)) => (winner, &as_integer, loser, &as_boolean),
        (Err(loser), Ok(winner)) => (winner, &as_boolean, loser, &as_integer),
        (left, right) => {
            panic!("expected exactly one migration winner, got {left:?} and {right:?}")
        }
    };
    assert_eq!(loser_error.kind(), ErrorKind::Conflict);
    assert_eq!(winner.state, MigrationState::Converging);
    assert_eq!(winner.transition_ids.len(), 1);

    let replay = migration.apply(winner_schema, false).await.unwrap();
    assert_eq!(replay.state, MigrationState::Converging);
    assert_eq!(replay.transition_ids, winner.transition_ids);

    let blocked = migration.plan(loser_schema).await.unwrap();
    assert!(blocked.program.statements.is_empty());
    assert_eq!(blocked.blocking.len(), 1);
    assert_eq!(
        blocked.blocking[0].kind,
        "active_schema_transition_conflict"
    );

    engine
        .cancel_schema_transition(&winner.transition_ids[0])
        .await
        .unwrap();
    let fresh = migration.plan(loser_schema).await.unwrap();
    assert!(fresh.blocking.is_empty());
    let restarted = migration.apply_plan(fresh, false).await.unwrap();
    assert_eq!(restarted.state, MigrationState::Converging);
    assert_eq!(restarted.transition_ids.len(), 1);
    assert_ne!(restarted.transition_ids, winner.transition_ids);
}

#[tokio::test]
async fn cancelled_dependency_graph_restarts_with_fresh_ids_and_edges() {
    let store = Arc::new(
        Store::memory("frontend-migration-restart-graph")
            .await
            .unwrap(),
    );
    let engine = Engine::new(store);
    let migration = Migration::new(&engine);
    let initial = parse(
        r#"
tables:
  - id: 1
    name: events
    columns:
      - { id: 1, name: id, type: int64, pk: true }
      - { id: 2, name: left_value, type: string }
      - { id: 3, name: right_value, type: string }
"#,
    );
    let desired = parse(
        r#"
tables:
  - id: 1
    name: events
    columns:
      - { id: 1, name: id, type: int64, pk: true }
      - { id: 2, name: left_value, type: int64 }
      - { id: 3, name: right_value, type: int64 }
    indexes:
      - { name: events_value_pair_uq, columns: [left_value, right_value], unique: true }
"#,
    );
    migration.apply(&initial, false).await.unwrap();

    let first = migration.apply(&desired, false).await.unwrap();
    assert_eq!(first.transition_ids.len(), 3);
    let mut first_replacements = Vec::new();
    let mut first_index = None;
    for transition_id in &first.transition_ids {
        let transition = engine
            .inspect_schema_transition(transition_id)
            .await
            .unwrap();
        match transition.kind {
            TransitionKind::ColumnReplacement => first_replacements.push(transition_id.clone()),
            TransitionKind::IndexBuild => first_index = Some(transition_id.clone()),
            kind => panic!("unexpected transition kind {kind:?}"),
        }
    }
    assert_eq!(first_replacements.len(), 2);
    let first_index = first_index.unwrap();
    engine.cancel_schema_transition(&first_index).await.unwrap();
    for transition_id in &first_replacements {
        engine
            .cancel_schema_transition(transition_id)
            .await
            .unwrap();
    }

    let second = migration.apply(&desired, false).await.unwrap();
    assert_eq!(second.transition_ids.len(), 3);
    assert!(
        second
            .transition_ids
            .iter()
            .all(|transition_id| !first.transition_ids.contains(transition_id))
    );
    let mut second_replacements = Vec::new();
    let mut second_index = None;
    for transition_id in &second.transition_ids {
        let transition = engine
            .inspect_schema_transition(transition_id)
            .await
            .unwrap();
        match transition.kind {
            TransitionKind::ColumnReplacement => second_replacements.push(transition_id.clone()),
            TransitionKind::IndexBuild => second_index = Some(transition),
            kind => panic!("unexpected transition kind {kind:?}"),
        }
    }
    second_replacements.sort();
    let mut prerequisites = second_index.unwrap().prerequisites;
    prerequisites.sort();
    assert_eq!(prerequisites, second_replacements);
}
