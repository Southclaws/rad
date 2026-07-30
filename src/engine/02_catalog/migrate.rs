//! Pure migration planner from a physical catalog to a desired schema.

use std::collections::{HashMap, HashSet};
use std::fmt;

use super::identity::{SchemaId, TableId};
use super::model::{
    ColumnConversion, ColumnReplacementDef, ConstraintDef, ConstraintKind, DefaultValue, IndexDef,
    Table, TableDef,
};
use super::schema::{Schema, Table as DesiredTable};
use super::{Error, ErrorKind, Result, naming};

#[derive(Clone, Debug, PartialEq)]
pub enum Step {
    RenameTable {
        from: String,
        to: String,
    },
    RenameColumn {
        table: String,
        from: String,
        to: String,
    },
    CreateTable {
        definition: TableDef,
    },
    CreateColumn {
        table: String,
        definition: super::model::ColumnDef,
    },
    ChangeColumnDefault {
        table: String,
        column: String,
        default: Option<DefaultValue>,
    },
    CreateIndex {
        table: String,
        definition: IndexDef,
    },
    ReplaceColumn {
        table: String,
        column: String,
        column_id: SchemaId,
        definition: ColumnReplacementDef,
    },
    ValidateNotNull {
        table: String,
        column: String,
        definition: ConstraintDef,
    },
    DeleteIndex {
        table: String,
        index: String,
    },
    DeleteColumn {
        table: String,
        column: String,
    },
    DeleteTable {
        table: String,
    },
}

impl fmt::Display for Step {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RenameTable { from, to } => write!(formatter, "rename table {from} -> {to}"),
            Self::RenameColumn { table, from, to } => {
                write!(formatter, "rename column {table}.{from} -> {to}")
            }
            Self::CreateTable { definition } => {
                write!(formatter, "create table {}", definition.name)
            }
            Self::CreateColumn { table, definition } => {
                write!(formatter, "create column {table}.{}", definition.name)
            }
            Self::ChangeColumnDefault { table, column, .. } => {
                write!(formatter, "change column default {table}.{column}")
            }
            Self::CreateIndex { table, definition } => write!(
                formatter,
                "start index build {} on {table}",
                definition.name
            ),
            Self::ReplaceColumn { table, column, .. } => {
                write!(formatter, "replace column {table}.{column}")
            }
            Self::ValidateNotNull { table, column, .. } => {
                write!(formatter, "validate not-null {table}.{column}")
            }
            Self::DeleteIndex { table, index } => {
                write!(formatter, "delete index {index} on {table}")
            }
            Self::DeleteColumn { table, column } => {
                write!(formatter, "delete column {table}.{column}")
            }
            Self::DeleteTable { table } => write!(formatter, "delete table {table}"),
        }
    }
}

pub fn diff(current: &[Table], desired: &Schema) -> Result<Vec<Step>> {
    let mut current_by_id = HashMap::new();
    let mut current_by_name = HashMap::new();
    let mut current_by_physical = HashMap::new();
    for table in current {
        if let Some(previous) = current_by_id.insert(table.schema_id, table) {
            return Err(drift(format!(
                "migrate: physical tables {:?} and {:?} share schema ID {}",
                previous.name, table.name, table.schema_id
            )));
        }
        current_by_name.insert(table.name.as_str(), table);
        current_by_physical.insert(table.id.clone(), table);
    }
    let mut desired_ids = HashMap::new();
    let mut desired_names = HashMap::new();
    for table in &desired.tables {
        if let Some(previous) = desired_ids.insert(table.def.id, table.def.name.as_str()) {
            return Err(input(format!(
                "migrate: tables {previous:?} and {:?} share schema ID {}",
                table.def.name, table.def.id
            )));
        }
        if let Some(previous) = desired_names.insert(table.def.name.as_str(), table.def.id) {
            return Err(input(format!(
                "migrate: tables with schema IDs {previous} and {} share name {:?}",
                table.def.id, table.def.name
            )));
        }
    }

    let mut renames = Vec::new();
    let mut creates = Vec::new();
    let mut matched = HashSet::new();
    for wanted in &desired.tables {
        let Some(existing) = current_by_id.get(&wanted.def.id) else {
            if let Some(occupied) = current_by_name.get(wanted.def.name.as_str()) {
                return Err(input(format!(
                    "migrate: table {:?} changes schema ID {} -> {}; remove it in one migration before creating the replacement",
                    wanted.def.name, occupied.schema_id, wanted.def.id
                )));
            }
            creates.push(wanted.clone());
            continue;
        };
        matched.insert(wanted.def.id);
        if existing.name != wanted.def.name {
            if let Some(occupied) = current_by_name.get(wanted.def.name.as_str())
                && occupied.schema_id != existing.schema_id
            {
                return Err(input(format!(
                    "migrate: cannot rename table {:?} to {:?} because that name belongs to schema ID {}",
                    existing.name, wanted.def.name, occupied.schema_id
                )));
            }
            renames.push(Step::RenameTable {
                from: existing.name.clone(),
                to: wanted.def.name.clone(),
            });
        }
    }

    let deleted_tables: Vec<_> = current
        .iter()
        .filter(|table| !matched.contains(&table.schema_id))
        .cloned()
        .collect();
    let deleted_names: HashSet<_> = deleted_tables
        .iter()
        .map(|table| table.name.as_str())
        .collect();
    for wanted in &desired.tables {
        for foreign_key in &wanted.def.foreign_keys {
            if deleted_names.contains(foreign_key.ref_table.as_str()) {
                return Err(input(format!(
                    "migrate: table {:?} references deleted table {:?}",
                    wanted.def.name, foreign_key.ref_table
                )));
            }
        }
    }

    let mut table_creates = order_creates(creates)?
        .into_iter()
        .map(|table| Step::CreateTable {
            definition: table.def,
        })
        .collect::<Vec<_>>();
    let mut adds = Vec::new();
    let mut defaults = Vec::new();
    let mut index_deletes = Vec::new();
    let mut replacements = Vec::new();
    let mut constraints = Vec::new();
    let mut index_creates = Vec::new();
    let mut column_deletes = Vec::new();
    for wanted in &desired.tables {
        let Some(existing) = current_by_id.get(&wanted.def.id) else {
            continue;
        };
        let groups = diff_table(existing, wanted, &current_by_physical, &desired_names)?;
        renames.extend(groups.renames);
        adds.extend(groups.adds);
        defaults.extend(groups.defaults);
        index_deletes.extend(groups.index_deletes);
        replacements.extend(groups.replacements);
        constraints.extend(groups.constraints);
        index_creates.extend(groups.index_creates);
        column_deletes.extend(groups.column_deletes);
    }
    for group in [
        &mut adds,
        &mut defaults,
        &mut index_deletes,
        &mut replacements,
        &mut constraints,
        &mut index_creates,
        &mut column_deletes,
    ] {
        group.sort_by_key(ToString::to_string);
    }
    let mut result = Vec::new();
    result.append(&mut renames);
    result.append(&mut table_creates);
    result.append(&mut adds);
    result.append(&mut defaults);
    result.append(&mut index_deletes);
    result.append(&mut replacements);
    result.append(&mut constraints);
    result.append(&mut index_creates);
    result.append(&mut column_deletes);
    result.extend(
        order_deletes(&deleted_tables)
            .into_iter()
            .map(|table| Step::DeleteTable { table: table.name }),
    );
    Ok(result)
}

#[derive(Default)]
struct Groups {
    renames: Vec<Step>,
    adds: Vec<Step>,
    defaults: Vec<Step>,
    index_deletes: Vec<Step>,
    replacements: Vec<Step>,
    constraints: Vec<Step>,
    index_creates: Vec<Step>,
    column_deletes: Vec<Step>,
}

fn diff_table(
    current: &Table,
    desired: &DesiredTable,
    current_by_physical: &HashMap<TableId, &Table>,
    desired_by_name: &HashMap<&str, SchemaId>,
) -> Result<Groups> {
    let name = &desired.def.name;
    let mut result = Groups::default();
    let mut current_by_id = HashMap::new();
    let mut current_by_name = HashMap::new();
    for column in &current.columns {
        if let Some(previous) = current_by_id.insert(column.schema_id, column) {
            return Err(drift(format!(
                "migrate: physical columns {:?}.{:?} and {:?}.{:?} share schema ID {}",
                current.name, previous.name, current.name, column.name, column.schema_id
            )));
        }
        current_by_name.insert(column.name.as_str(), column);
    }
    let mut desired_ids = HashMap::new();
    let mut desired_names = HashMap::new();
    for column in &desired.def.columns {
        if let Some(previous) = desired_ids.insert(column.id, column.name.as_str()) {
            return Err(input(format!(
                "migrate: columns {name:?}.{previous:?} and {name:?}.{:?} share schema ID {}",
                column.name, column.id
            )));
        }
        if let Some(previous) = desired_names.insert(column.name.as_str(), column.id) {
            return Err(input(format!(
                "migrate: columns with schema IDs {previous} and {} on table {name:?} share name {:?}",
                column.id, column.name
            )));
        }
    }
    for wanted in &desired.def.columns {
        let Some(existing) = current_by_id.get(&wanted.id) else {
            if let Some(occupied) = current_by_name.get(wanted.name.as_str()) {
                return Err(input(format!(
                    "migrate: column {name:?}.{:?} changes schema ID {} -> {}; remove it in one migration before creating the replacement",
                    wanted.name, occupied.schema_id, wanted.id
                )));
            }
            result.adds.push(Step::CreateColumn {
                table: name.clone(),
                definition: wanted.clone(),
            });
            continue;
        };
        if existing.name != wanted.name {
            if let Some(occupied) = current_by_name.get(wanted.name.as_str())
                && occupied.schema_id != existing.schema_id
            {
                return Err(input(format!(
                    "migrate: cannot rename column {name:?}.{:?} to {:?} because that name belongs to schema ID {}",
                    existing.name, wanted.name, occupied.schema_id
                )));
            }
            result.renames.push(Step::RenameColumn {
                table: name.clone(),
                from: existing.name.clone(),
                to: wanted.name.clone(),
            });
        }
    }
    for wanted in &desired.def.columns {
        let Some(existing) = current_by_id.get(&wanted.id) else {
            continue;
        };
        let default_changed = existing.insert_default != wanted.default;
        let physical_change = existing.scalar_type != wanted.scalar_type
            || existing.format != wanted.format
            || (!existing.nullable && wanted.nullable);
        if physical_change {
            result.replacements.push(Step::ReplaceColumn {
                table: name.clone(),
                column: wanted.name.clone(),
                column_id: wanted.id,
                definition: ColumnReplacementDef {
                    scalar_type: wanted.scalar_type,
                    nullable: wanted.nullable,
                    format: wanted.format.clone(),
                    default: wanted.default.clone(),
                    conversion: ColumnConversion::StrictBuiltin,
                    prerequisites: Vec::new(),
                },
            });
        } else if existing.nullable && !wanted.nullable {
            if default_changed {
                result.defaults.push(Step::ChangeColumnDefault {
                    table: name.clone(),
                    column: wanted.name.clone(),
                    default: wanted.default.clone(),
                });
            }
            result.constraints.push(Step::ValidateNotNull {
                table: name.clone(),
                column: wanted.name.clone(),
                definition: ConstraintDef {
                    name: naming::not_null_constraint(name, &wanted.name),
                    kind: ConstraintKind::NotNull,
                    column_id: wanted.id,
                    prerequisites: Vec::new(),
                },
            });
        } else if default_changed {
            result.defaults.push(Step::ChangeColumnDefault {
                table: name.clone(),
                column: wanted.name.clone(),
                default: wanted.default.clone(),
            });
        }
    }
    let current_order: Vec<_> = current
        .columns
        .iter()
        .filter(|column| desired_ids.contains_key(&column.schema_id))
        .map(|column| column.schema_id)
        .collect();
    let desired_order: Vec<_> = desired
        .def
        .columns
        .iter()
        .filter(|column| current_by_id.contains_key(&column.id))
        .map(|column| column.id)
        .collect();
    if current_order != desired_order {
        return Err(input(format!(
            "migrate: {name}: changing column order is not supported"
        )));
    }
    for column in &current.columns {
        if !desired_ids.contains_key(&column.schema_id) {
            result.column_deletes.push(Step::DeleteColumn {
                table: name.clone(),
                column: column.name.clone(),
            });
        }
    }
    let mut renamed_primary_key = current.primary_key.clone();
    apply_renames(&mut renamed_primary_key, &result.renames);
    if renamed_primary_key != desired.def.primary_key {
        return Err(input(format!(
            "migrate: {name}: changing the primary key is not supported"
        )));
    }
    compare_foreign_keys(
        current,
        desired,
        &result.renames,
        current_by_physical,
        desired_by_name,
    )?;

    let mut current_indexes = HashMap::new();
    for index in current.indexes.iter().filter(|index| index.is_ready()) {
        let mut index = index.clone();
        let generated = index.name == naming::index(&current.name, &index.columns, index.unique);
        apply_renames(&mut index.columns, &result.renames);
        if generated {
            index.name = naming::index(name, &index.columns, index.unique);
        }
        current_indexes.insert(index_signature(&index.columns, index.unique), index);
    }
    let desired_indexes: HashMap<_, _> = desired
        .def
        .indexes
        .iter()
        .map(|index| (index_signature(&index.columns, index.unique), index))
        .collect();
    let replaced: HashSet<_> = result
        .replacements
        .iter()
        .filter_map(|step| match step {
            Step::ReplaceColumn { column, .. } => Some(column.as_str()),
            _ => None,
        })
        .collect();
    for (signature, wanted) in &desired_indexes {
        let existing = current_indexes.get(signature);
        if let Some(existing) = existing
            && existing.name != wanted.name
        {
            return Err(input(format!(
                "migrate: {name}: renaming index {:?} to {:?} is not supported",
                existing.name, wanted.name
            )));
        }
        let rebuild = wanted
            .columns
            .iter()
            .any(|column| replaced.contains(column.as_str()));
        if let Some(existing) = existing
            && rebuild
        {
            result.index_deletes.push(Step::DeleteIndex {
                table: name.clone(),
                index: existing.name.clone(),
            });
            result.index_creates.push(Step::CreateIndex {
                table: name.clone(),
                definition: (*wanted).clone(),
            });
        } else if existing.is_none() {
            result.index_creates.push(Step::CreateIndex {
                table: name.clone(),
                definition: (*wanted).clone(),
            });
        }
    }
    for (signature, existing) in current_indexes {
        if !desired_indexes.contains_key(&signature) {
            result.index_deletes.push(Step::DeleteIndex {
                table: name.clone(),
                index: existing.name,
            });
        }
    }
    for group in [
        &mut result.renames,
        &mut result.adds,
        &mut result.defaults,
        &mut result.index_deletes,
        &mut result.replacements,
        &mut result.constraints,
        &mut result.index_creates,
        &mut result.column_deletes,
    ] {
        group.sort_by_key(ToString::to_string);
    }
    Ok(result)
}

fn apply_renames(names: &mut [String], steps: &[Step]) {
    for step in steps {
        if let Step::RenameColumn { from, to, .. } = step {
            for name in &mut *names {
                if name == from {
                    name.clone_from(to);
                }
            }
        }
    }
}

fn compare_foreign_keys(
    current: &Table,
    desired: &DesiredTable,
    renames: &[Step],
    current_by_physical: &HashMap<TableId, &Table>,
    desired_by_name: &HashMap<&str, SchemaId>,
) -> Result<()> {
    let mut current_set = HashSet::new();
    for foreign_key in &current.foreign_keys {
        let referenced = current_by_physical.get(&foreign_key.ref_table_id).ok_or_else(|| drift(format!("migrate: foreign key {:?} on table {:?} references missing physical table ID {:?}", foreign_key.name, current.name, foreign_key.ref_table_id)))?;
        let mut columns = foreign_key.columns.clone();
        let mut name = foreign_key.name.clone();
        let generated =
            columns.len() == 1 && name == naming::foreign_key(&current.name, &columns[0]);
        apply_renames(&mut columns, renames);
        if generated {
            name = naming::foreign_key(&desired.def.name, &columns[0]);
        }
        current_set.insert((name, columns, referenced.schema_id));
    }
    let mut desired_set = HashSet::new();
    for foreign_key in &desired.def.foreign_keys {
        let referenced = desired_by_name
            .get(foreign_key.ref_table.as_str())
            .ok_or_else(|| {
                input(format!(
                    "migrate: foreign key {:?} on table {:?} references unknown table {:?}",
                    foreign_key.name, desired.def.name, foreign_key.ref_table
                ))
            })?;
        desired_set.insert((
            foreign_key.name.clone(),
            foreign_key.columns.clone(),
            *referenced,
        ));
    }
    if current_set != desired_set {
        return Err(input(format!(
            "migrate: {}: changing foreign keys on an existing table is not supported",
            desired.def.name
        )));
    }
    Ok(())
}

fn order_creates(mut creates: Vec<DesiredTable>) -> Result<Vec<DesiredTable>> {
    creates.sort_by(|left, right| left.def.name.cmp(&right.def.name));
    let names: HashSet<_> = creates
        .iter()
        .map(|table| table.def.name.as_str())
        .collect();
    let mut done = HashSet::new();
    let mut result = Vec::new();
    while result.len() < creates.len() {
        let mut progressed = false;
        for table in &creates {
            if done.contains(table.def.name.as_str()) {
                continue;
            }
            let ready = table.def.foreign_keys.iter().all(|foreign_key| {
                foreign_key.ref_table == table.def.name
                    || !names.contains(foreign_key.ref_table.as_str())
                    || done.contains(foreign_key.ref_table.as_str())
            });
            if ready {
                result.push(table.clone());
                done.insert(table.def.name.as_str());
                progressed = true;
            }
        }
        if !progressed {
            return Err(input(
                "migrate: circular foreign key dependency among new tables",
            ));
        }
    }
    Ok(result)
}

fn order_deletes(tables: &[Table]) -> Vec<Table> {
    let by_id: HashMap<_, _> = tables
        .iter()
        .map(|table| (table.id.clone(), table.name.as_str()))
        .collect();
    let mut references = HashMap::<&str, usize>::new();
    for table in tables {
        for foreign_key in &table.foreign_keys {
            if foreign_key.ref_table_id != table.id
                && let Some(parent) = by_id.get(&foreign_key.ref_table_id)
            {
                *references.entry(parent).or_default() += 1;
            }
        }
    }
    let mut done = HashSet::new();
    let mut result = Vec::new();
    while result.len() < tables.len() {
        let mut progressed = false;
        for table in tables {
            if done.contains(table.name.as_str())
                || references
                    .get(table.name.as_str())
                    .copied()
                    .unwrap_or_default()
                    > 0
            {
                continue;
            }
            result.push(table.clone());
            done.insert(table.name.as_str());
            progressed = true;
            for foreign_key in &table.foreign_keys {
                if foreign_key.ref_table_id != table.id
                    && let Some(parent) = by_id.get(&foreign_key.ref_table_id)
                {
                    *references.entry(parent).or_default() -= 1;
                }
            }
        }
        if !progressed {
            result.extend(
                tables
                    .iter()
                    .filter(|table| !done.contains(table.name.as_str()))
                    .cloned(),
            );
            break;
        }
    }
    result
}

fn index_signature(columns: &[String], unique: bool) -> String {
    format!("{}{}", columns.join(","), if unique { "!" } else { "" })
}

fn input(message: impl Into<String>) -> Error {
    Error::message(ErrorKind::InvalidInput, message)
}
fn drift(message: impl Into<String>) -> Error {
    Error::message(ErrorKind::CatalogDrift, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::catalog::identity::{
        AccessGeneration, ColumnId, DefinitionGeneration, ExistenceGeneration, IndexId,
        LogicalIndexId, ValueGeneration, WriteProtocolGeneration,
    };
    use crate::engine::catalog::model::{Column, ForeignKey, Index, IndexState};

    fn desired(source: &str) -> Schema {
        super::super::schema::parse("test.rad", source.as_bytes()).unwrap()
    }

    fn physical(source: &str) -> Vec<Table> {
        desired(source)
            .tables
            .into_iter()
            .map(|wanted| {
                let id: TableId = format!("t{}", wanted.def.id.get()).into();
                Table {
                    id,
                    schema_id: wanted.def.id,
                    name: wanted.def.name,
                    definition_generation: DefinitionGeneration::ZERO,
                    existence_generation: ExistenceGeneration::ZERO,
                    write_protocol_generation: WriteProtocolGeneration::ZERO,
                    columns: wanted
                        .def
                        .columns
                        .into_iter()
                        .map(|column| Column {
                            id: ColumnId::from(format!("c{}", column.id.get())),
                            schema_id: column.id,
                            name: column.name,
                            value_generation: ValueGeneration::ZERO,
                            scalar_type: column.scalar_type,
                            nullable: column.nullable,
                            format: column.format,
                            insert_default: column.default.clone(),
                            missing_value: column.default.filter(|value| value.function.is_none()),
                        })
                        .collect(),
                    primary_key: wanted.def.primary_key,
                    indexes: wanted
                        .def
                        .indexes
                        .into_iter()
                        .enumerate()
                        .map(|(offset, index)| Index {
                            id: IndexId::from(format!("i{offset}")),
                            logical_id: LogicalIndexId::from(format!("ix{offset}")),
                            access_generation: AccessGeneration::ZERO,
                            definition_generation: DefinitionGeneration::ZERO,
                            state: IndexState::Ready,
                            name: index.name,
                            columns: index.columns,
                            column_ids: Vec::new(),
                            unique: index.unique,
                        })
                        .collect(),
                    foreign_keys: wanted
                        .def
                        .foreign_keys
                        .into_iter()
                        .enumerate()
                        .map(|(offset, foreign_key)| ForeignKey {
                            id: format!("fk{offset}"),
                            name: foreign_key.name,
                            columns: foreign_key.columns,
                            ref_table_id: TableId::from(format!(
                                "t{}",
                                desired(source)
                                    .table(&foreign_key.ref_table)
                                    .unwrap()
                                    .def
                                    .id
                                    .get()
                            )),
                            ref_columns: foreign_key.ref_columns,
                        })
                        .collect(),
                    constraints: Vec::new(),
                }
            })
            .collect()
    }

    const V1: &str = r#"
tables:
  - id: 1
    name: users
    columns:
      - { id: 1, name: id, type: string, pk: true }
      - { id: 2, name: name, type: string, index: true }
  - id: 2
    name: boards
    columns:
      - { id: 1, name: id, type: string, pk: true }
      - { id: 2, name: user_id, type: string, ref: users.id }
"#;

    fn strings(steps: &[Step]) -> Vec<String> {
        steps.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn initial_plan_is_dependency_ordered_and_idempotent() {
        assert_eq!(
            strings(&diff(&[], &desired(V1)).unwrap()),
            ["create table users", "create table boards"]
        );
        assert!(diff(&physical(V1), &desired(V1)).unwrap().is_empty());
    }

    #[test]
    fn stable_id_renames_without_rebuilding_generated_index() {
        let wanted = V1
            .replace("name: users", "name: people")
            .replace("name: name", "name: full_name")
            .replace("ref: users.id", "ref: people.id");
        assert_eq!(
            strings(&diff(&physical(V1), &desired(&wanted)).unwrap()),
            [
                "rename table users -> people",
                "rename column people.name -> full_name"
            ]
        );
    }

    #[test]
    fn semantic_column_changes_become_online_steps() {
        let initial = r#"tables: [{id: 1, name: events, columns: [{id: 1, name: id, type: int64, pk: true}, {id: 2, name: value, type: string, nullable: true, index: true}]}]"#;
        let replacement = initial.replace("type: string", "type: int64");
        assert_eq!(
            strings(&diff(&physical(initial), &desired(&replacement)).unwrap()),
            [
                "delete index events_value_idx on events",
                "replace column events.value",
                "start index build events_value_idx on events"
            ]
        );
        let not_null = initial.replace(", nullable: true, index: true", ", index: true");
        assert_eq!(
            strings(&diff(&physical(initial), &desired(&not_null)).unwrap()),
            ["validate not-null events.value"]
        );
    }

    #[test]
    fn rejects_identity_primary_key_and_dependency_cycles() {
        let changed_id = V1.replacen("id: 2, name: name", "id: 3, name: name", 1);
        assert!(
            diff(&physical(V1), &desired(&changed_id))
                .unwrap_err()
                .to_string()
                .contains("changes schema ID")
        );
        let cycle = desired(
            r#"tables:
          - {id: 1, name: a, columns: [{id: 1, name: id, type: string, pk: true}, {id: 2, name: b_id, type: string, ref: b.id}]}
          - {id: 2, name: b, columns: [{id: 1, name: id, type: string, pk: true}, {id: 2, name: a_id, type: string, ref: a.id}]}
        "#,
        );
        assert!(
            diff(&[], &cycle)
                .unwrap_err()
                .to_string()
                .contains("circular")
        );
    }

    #[test]
    fn plans_adds_and_referencing_table_first_deletes() {
        let wanted = V1.replace(
            "- { id: 2, name: name, type: string, index: true }",
            "- { id: 2, name: name, type: string, index: true }\n      - { id: 3, name: email, type: string, nullable: true, default: unknown, unique: true }",
        );
        assert_eq!(
            strings(&diff(&physical(V1), &desired(&wanted)).unwrap()),
            [
                "create column users.email",
                "start index build users_email_uq on users"
            ]
        );
        assert_eq!(
            strings(&diff(&physical(V1), &desired("tables: []")).unwrap()),
            ["delete table boards", "delete table users"]
        );
    }

    #[test]
    fn plans_default_changes_and_referenced_primary_key_renames_by_identity() {
        let defaults = r#"tables: [{id: 1, name: items, columns: [{id: 1, name: id, type: int64, pk: true}, {id: 2, name: status, type: string, nullable: true, default: active}]}]"#;
        let changed = defaults.replace("default: active", "default: pending");
        assert_eq!(
            strings(&diff(&physical(defaults), &desired(&changed)).unwrap()),
            ["change column default items.status"]
        );

        let initial = r#"tables:
          - {id: 1, name: parents, columns: [{id: 1, name: id, type: string, pk: true}]}
          - {id: 2, name: children, columns: [{id: 1, name: id, type: string, pk: true}, {id: 2, name: parent_id, type: string, ref: parents.id}]}
        "#;
        let renamed = initial
            .replacen(
                "name: id, type: string, pk: true}]}",
                "name: parent_key, type: string, pk: true}]}",
                1,
            )
            .replace("ref: parents.id", "ref: parents.parent_key");
        assert_eq!(
            strings(&diff(&physical(initial), &desired(&renamed)).unwrap()),
            ["rename column parents.id -> parent_key"]
        );
    }

    #[test]
    fn rejects_surviving_references_and_existing_key_shape_changes() {
        let deleting_parent = r#"tables:
          - {id: 2, name: boards, columns: [{id: 1, name: id, type: string, pk: true}, {id: 2, name: user_id, type: string, ref: users.id}]}
        "#;
        assert!(
            diff(&physical(V1), &desired(deleting_parent))
                .unwrap_err()
                .to_string()
                .contains("references deleted table")
        );

        let changed_primary_key = V1
            .replace("name: id, type: string, pk: true", "name: id, type: string")
            .replace(
                "name: name, type: string, index: true",
                "name: name, type: string, pk: true, index: true",
            );
        assert!(
            diff(&physical(V1), &desired(&changed_primary_key))
                .unwrap_err()
                .to_string()
                .contains("primary key")
        );

        let removed_foreign_key = V1.replace(", ref: users.id", "");
        assert!(
            diff(&physical(V1), &desired(&removed_foreign_key))
                .unwrap_err()
                .to_string()
                .contains("foreign keys")
        );
    }
}
