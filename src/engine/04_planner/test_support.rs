use crate::engine::catalog::identity::{
    AccessGeneration, DefinitionGeneration, ExistenceGeneration, LogicalIndexId, SchemaId,
    ValueGeneration, WriteProtocolGeneration,
};
use crate::engine::catalog::model::{Column, Index, IndexState, ScalarType, Table};
use crate::engine::lir::bound;
use crate::engine::lir::{RootCardinality, SlotId};

pub fn table() -> Table {
    Table {
        id: "tasks-table".into(),
        schema_id: SchemaId::new(1).unwrap(),
        name: "tasks".into(),
        definition_generation: DefinitionGeneration::ZERO,
        existence_generation: ExistenceGeneration::from(2),
        write_protocol_generation: WriteProtocolGeneration::ZERO,
        columns: ["id", "board_id", "status"]
            .into_iter()
            .enumerate()
            .map(|(index, name)| Column {
                id: format!("column-{name}").into(),
                schema_id: SchemaId::new(index as u32 + 1).unwrap(),
                name: name.into(),
                value_generation: ValueGeneration::from(index as u64 + 3),
                scalar_type: ScalarType::Text,
                nullable: false,
                format: String::new(),
                insert_default: None,
                missing_value: None,
            })
            .collect(),
        primary_key: vec!["id".into()],
        indexes: vec![Index {
            id: "board-status-index".into(),
            logical_id: LogicalIndexId::from("board-status"),
            definition_generation: DefinitionGeneration::ZERO,
            access_generation: AccessGeneration::from(7),
            state: IndexState::Ready,
            name: "tasks_board_status_idx".into(),
            columns: vec!["board_id".into(), "status".into()],
            column_ids: vec!["column-board_id".into(), "column-status".into()],
            unique: false,
        }],
        foreign_keys: Vec::new(),
        constraints: Vec::new(),
    }
}

pub fn scan() -> bound::Relation {
    bound::Relation::scan(table(), "t", vec![SlotId(0), SlotId(1), SlotId(2)])
}

pub fn column(relation: &bound::Relation, name: &str) -> bound::Expr {
    let field = relation.output().lookup(name).unwrap();
    bound::Expr::slot(field.slot, format!("t.{name}"), field.value_type.clone())
}

pub fn query(root: bound::Relation, next_slot: usize) -> bound::Query {
    bound::Query {
        root,
        cardinality: RootCardinality::Many,
        bindings: Vec::new(),
        next_slot: SlotId(next_slot),
    }
}
