use std::collections::HashMap;

use crate::engine::catalog::identity::SchemaId;
use crate::engine::catalog::migrate::Step;
use crate::engine::catalog::model::{Schema, TableDef};
use crate::engine::exec::{DefaultSpec, Error, ErrorKind, Program, Result, Statement};

pub(super) fn lower(current: &Schema, steps: &[Step]) -> Result<Program> {
    let mut tables = current
        .tables
        .iter()
        .cloned()
        .map(|table| (table.name.clone(), table))
        .collect::<HashMap<_, _>>();
    let mut replacement_by_column = HashMap::<(SchemaId, SchemaId), String>::new();
    let mut statements = Vec::with_capacity(steps.len());
    for (position, step) in steps.iter().enumerate() {
        let name = format!("migration_{}", position + 1);
        let statement = match step {
            Step::RenameTable { from, to } => {
                let mut table = take_table(&mut tables, from)?;
                let statement = Statement::RenameTable {
                    name,
                    table_id: table.id,
                    to: to.clone(),
                };
                table.name.clone_from(to);
                tables.insert(to.clone(), table);
                statement
            }
            Step::RenameColumn { table, from, to } => {
                let mut logical = take_table(&mut tables, table)?;
                let column = logical.column(from).cloned().ok_or_else(|| {
                    input(format!(
                        "migration: column {table:?}.{from:?} does not exist"
                    ))
                })?;
                let statement = Statement::RenameColumn {
                    name,
                    table_id: logical.id,
                    column_id: column.id,
                    to: to.clone(),
                };
                logical
                    .columns
                    .iter_mut()
                    .find(|candidate| candidate.id == column.id)
                    .expect("column cloned from logical table")
                    .name
                    .clone_from(to);
                tables.insert(table.clone(), logical);
                statement
            }
            Step::CreateTable { definition } => {
                let logical = definition.clone();
                tables.insert(logical.name.clone(), logical);
                Statement::CreateTable {
                    name,
                    table: definition.clone().into(),
                }
            }
            Step::CreateColumn { table, definition } => {
                let mut logical = take_table(&mut tables, table)?;
                let statement = Statement::CreateColumn {
                    name,
                    table_id: logical.id,
                    column: definition.clone().into(),
                };
                logical.columns.push(definition.clone());
                tables.insert(table.clone(), logical);
                statement
            }
            Step::ChangeColumnDefault {
                table,
                column,
                default,
            } => {
                let logical = required_table(&tables, table)?;
                let column = logical.column(column).ok_or_else(|| {
                    input(format!(
                        "migration: column {table:?}.{column:?} does not exist"
                    ))
                })?;
                Statement::ChangeColumnDefault {
                    name,
                    table_id: logical.id,
                    column_id: column.id,
                    default: default
                        .as_ref()
                        .map(|value| DefaultSpec::from_catalog(value, column.scalar_type)),
                }
            }
            Step::CreateIndex { table, definition } => {
                let logical = required_table(&tables, table)?;
                let mut after = Vec::new();
                for column_name in &definition.columns {
                    let column = logical.column(column_name).ok_or_else(|| {
                        input(format!(
                            "migration: index {:?} column {table:?}.{column_name:?} does not exist",
                            definition.name
                        ))
                    })?;
                    if let Some(prerequisite) = replacement_by_column.get(&(logical.id, column.id))
                    {
                        after.push(prerequisite.clone());
                    }
                }
                after.sort();
                after.dedup();
                Statement::StartIndexBuild {
                    name,
                    table_id: logical.id,
                    index: definition.clone(),
                    prerequisites: Vec::new(),
                    after,
                }
            }
            Step::ReplaceColumn {
                table,
                column,
                definition,
                ..
            } => {
                let logical = required_table(&tables, table)?;
                let column = logical.column(column).ok_or_else(|| {
                    input(format!(
                        "migration: column {table:?}.{column:?} does not exist"
                    ))
                })?;
                replacement_by_column.insert((logical.id, column.id), name.clone());
                Statement::StartColumnReplacement {
                    name,
                    table_id: logical.id,
                    column_id: column.id,
                    replacement: definition.clone(),
                    after: Vec::new(),
                }
            }
            Step::ValidateNotNull {
                table, definition, ..
            } => {
                let logical = required_table(&tables, table)?;
                let after = replacement_by_column
                    .get(&(logical.id, definition.column_id))
                    .cloned()
                    .into_iter()
                    .collect();
                Statement::StartConstraintValidation {
                    name,
                    table_id: logical.id,
                    constraint: definition.clone(),
                    after,
                }
            }
            Step::DeleteIndex { table, index } => Statement::DeleteIndex {
                name,
                table_id: required_table(&tables, table)?.id,
                index: index.clone(),
            },
            Step::DeleteColumn { table, column } => {
                let mut logical = take_table(&mut tables, table)?;
                let id = logical
                    .column(column)
                    .map(|column| column.id)
                    .ok_or_else(|| {
                        input(format!(
                            "migration: column {table:?}.{column:?} does not exist"
                        ))
                    })?;
                logical.columns.retain(|candidate| candidate.id != id);
                let statement = Statement::DeleteColumn {
                    name,
                    table_id: logical.id,
                    column_id: id,
                };
                tables.insert(table.clone(), logical);
                statement
            }
            Step::DeleteTable { table } => Statement::DeleteTable {
                name,
                table_id: take_table(&mut tables, table)?.id,
            },
        };
        statements.push(statement);
    }
    Ok(Program {
        statements,
        result: None,
    })
}

fn required_table<'a>(tables: &'a HashMap<String, TableDef>, name: &str) -> Result<&'a TableDef> {
    tables
        .get(name)
        .ok_or_else(|| input(format!("migration: table {name:?} does not exist")))
}

fn take_table(tables: &mut HashMap<String, TableDef>, name: &str) -> Result<TableDef> {
    tables
        .remove(name)
        .ok_or_else(|| input(format!("migration: table {name:?} does not exist")))
}

fn input(message: impl Into<String>) -> Error {
    Error::message(ErrorKind::InvalidInput, message)
}

#[cfg(test)]
mod tests {
    use crate::engine::catalog::identity::{
        DefinitionGeneration, ExistenceGeneration, SchemaId, ValueGeneration,
        WriteProtocolGeneration,
    };
    use crate::engine::catalog::migrate::Step;
    use crate::engine::catalog::model::{
        Column, ColumnConversion, ColumnReplacementDef, ConstraintDef, ConstraintKind, IndexDef,
        ScalarType, Schema, Table,
    };
    use crate::engine::exec::Statement;

    use super::lower;

    fn id(value: u32) -> SchemaId {
        SchemaId::new(value).unwrap()
    }

    fn table(table_id: u32, name: &str, columns: &[(u32, &str)]) -> Table {
        Table {
            id: format!("physical-{table_id}").into(),
            schema_id: id(table_id),
            name: name.into(),
            definition_generation: DefinitionGeneration::ZERO,
            existence_generation: ExistenceGeneration::ZERO,
            write_protocol_generation: WriteProtocolGeneration::ZERO,
            columns: columns
                .iter()
                .map(|(column_id, name)| Column {
                    id: format!("physical-{table_id}-{column_id}").into(),
                    schema_id: id(*column_id),
                    name: (*name).into(),
                    value_generation: ValueGeneration::ZERO,
                    scalar_type: ScalarType::Text,
                    nullable: false,
                    format: String::new(),
                    insert_default: None,
                    missing_value: None,
                })
                .collect(),
            primary_key: Vec::new(),
            indexes: Vec::new(),
            foreign_keys: Vec::new(),
            constraints: Vec::new(),
        }
    }

    fn replacement() -> ColumnReplacementDef {
        ColumnReplacementDef {
            scalar_type: ScalarType::Int64,
            nullable: false,
            format: String::new(),
            default: None,
            conversion: ColumnConversion::StrictBuiltin,
            prerequisites: Vec::new(),
        }
    }

    #[test]
    fn dependencies_follow_stable_column_identity_through_renames() {
        let current = Schema::from_physical(&[table(
            41,
            "events",
            &[(1, "id"), (2, "left_value"), (3, "right_value")],
        )])
        .unwrap();
        let steps = vec![
            Step::RenameTable {
                from: "events".into(),
                to: "measurements".into(),
            },
            Step::RenameColumn {
                table: "measurements".into(),
                from: "left_value".into(),
                to: "minimum".into(),
            },
            Step::ReplaceColumn {
                table: "measurements".into(),
                column: "minimum".into(),
                column_id: id(2),
                definition: replacement(),
            },
            Step::ReplaceColumn {
                table: "measurements".into(),
                column: "right_value".into(),
                column_id: id(3),
                definition: replacement(),
            },
            Step::ValidateNotNull {
                table: "measurements".into(),
                column: "minimum".into(),
                definition: ConstraintDef {
                    name: "measurements_minimum_not_null".into(),
                    kind: ConstraintKind::NotNull,
                    column_id: id(2),
                    prerequisites: Vec::new(),
                },
            },
            Step::CreateIndex {
                table: "measurements".into(),
                definition: IndexDef {
                    name: "measurements_range_uq".into(),
                    columns: vec!["minimum".into(), "right_value".into(), "minimum".into()],
                    unique: true,
                },
            },
        ];

        let program = lower(&current, &steps).unwrap();
        assert_eq!(program.statements.len(), steps.len());
        assert!(matches!(
            &program.statements[2],
            Statement::StartColumnReplacement { table_id, column_id, .. }
                if *table_id == id(41) && *column_id == id(2)
        ));
        assert!(matches!(
            &program.statements[4],
            Statement::StartConstraintValidation { table_id, after, .. }
                if *table_id == id(41) && after == &["migration_3"]
        ));
        assert!(matches!(
            &program.statements[5],
            Statement::StartIndexBuild { table_id, after, .. }
                if *table_id == id(41) && after == &["migration_3", "migration_4"]
        ));
    }

    #[test]
    fn replacement_dependencies_do_not_leak_across_tables() {
        let current = Schema::from_physical(&[
            table(1, "left", &[(1, "value")]),
            table(2, "right", &[(1, "value")]),
        ])
        .unwrap();
        let program = lower(
            &current,
            &[
                Step::ReplaceColumn {
                    table: "left".into(),
                    column: "value".into(),
                    column_id: id(1),
                    definition: replacement(),
                },
                Step::CreateIndex {
                    table: "right".into(),
                    definition: IndexDef {
                        name: "right_value_idx".into(),
                        columns: vec!["value".into()],
                        unique: false,
                    },
                },
            ],
        )
        .unwrap();
        assert!(matches!(
            &program.statements[1],
            Statement::StartIndexBuild { table_id, after, .. }
                if *table_id == id(2) && after.is_empty()
        ));
    }
}
