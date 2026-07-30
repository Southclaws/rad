use std::collections::{HashMap, HashSet};

use crate::codegen::Result;
use crate::engine::catalog::model::{ScalarType, Schema};

/// Language-neutral schema facts consumed by every client generator.
///
/// Names here are wire/schema names. Exported identifiers, package names,
/// reserved-word escaping, scalar spellings, and pluralisation belong to the
/// language implementation, never this model.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Model {
    pub tables: Vec<Table>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Table {
    pub name: String,
    pub columns: Vec<Column>,
    pub primary_key: Vec<String>,
    pub unique_indexes: Vec<Vec<String>>,
    pub forward_relations: Vec<Relation>,
    pub reverse_relations: Vec<Relation>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Column {
    pub name: String,
    pub kind: ScalarKind,
    pub nullable: bool,
    pub has_default: bool,
    pub primary_key: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ScalarKind {
    Text,
    Int64,
    Float64,
    Bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Relation {
    pub foreign_key: String,
    pub source_table: String,
    pub target_table: String,
    pub source_columns: Vec<String>,
    /// `(scanned-side column, enclosing-side column)` pairs used to build the
    /// correlated relation in this direction.
    pub pairs: Vec<(String, String)>,
}

impl Model {
    pub fn from_schema(schema: &Schema) -> Result<Self> {
        let table_names = schema
            .tables
            .iter()
            .map(|table| table.name.as_str())
            .collect::<HashSet<_>>();
        let mut tables = Vec::with_capacity(schema.tables.len());
        let mut by_name = HashMap::with_capacity(schema.tables.len());

        for definition in &schema.tables {
            let primary_key = definition
                .primary_key
                .iter()
                .map(String::as_str)
                .collect::<HashSet<_>>();
            let columns = definition
                .columns
                .iter()
                .map(|column| Column {
                    name: column.name.clone(),
                    kind: column.scalar_type.into(),
                    nullable: column.nullable,
                    has_default: column.default.is_some(),
                    primary_key: primary_key.contains(column.name.as_str()),
                })
                .collect();
            let unique_indexes = definition
                .indexes
                .iter()
                .filter(|index| index.unique)
                .map(|index| index.columns.clone())
                .collect();
            by_name.insert(definition.name.clone(), tables.len());
            tables.push(Table {
                name: definition.name.clone(),
                columns,
                primary_key: definition.primary_key.clone(),
                unique_indexes,
                forward_relations: Vec::new(),
                reverse_relations: Vec::new(),
            });
        }

        for definition in &schema.tables {
            let source_index = by_name[&definition.name];
            for foreign_key in &definition.foreign_keys {
                if !table_names.contains(foreign_key.ref_table.as_str()) {
                    return Err(format!(
                        "codegen: table {:?} references unknown table {:?}",
                        definition.name, foreign_key.ref_table
                    )
                    .into());
                }
                if foreign_key.columns.len() != foreign_key.ref_columns.len() {
                    return Err(format!(
                        "codegen: foreign key {:?} has {} source and {} target columns",
                        foreign_key.name,
                        foreign_key.columns.len(),
                        foreign_key.ref_columns.len()
                    )
                    .into());
                }
                let target_index = by_name[&foreign_key.ref_table];
                let forward = Relation {
                    foreign_key: foreign_key.name.clone(),
                    source_table: definition.name.clone(),
                    target_table: foreign_key.ref_table.clone(),
                    source_columns: foreign_key.columns.clone(),
                    pairs: foreign_key
                        .ref_columns
                        .iter()
                        .cloned()
                        .zip(foreign_key.columns.iter().cloned())
                        .collect(),
                };
                let reverse = Relation {
                    foreign_key: foreign_key.name.clone(),
                    source_table: definition.name.clone(),
                    target_table: foreign_key.ref_table.clone(),
                    source_columns: foreign_key.columns.clone(),
                    pairs: foreign_key
                        .columns
                        .iter()
                        .cloned()
                        .zip(foreign_key.ref_columns.iter().cloned())
                        .collect(),
                };
                tables[source_index].forward_relations.push(forward);
                tables[target_index].reverse_relations.push(reverse);
            }
        }

        Ok(Self { tables })
    }
}

impl From<ScalarType> for ScalarKind {
    fn from(value: ScalarType) -> Self {
        match value {
            ScalarType::Text => Self::Text,
            ScalarType::Int64 => Self::Int64,
            ScalarType::Float64 => Self::Float64,
            ScalarType::Bool => Self::Bool,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::catalog::schema;

    #[test]
    fn resolves_both_relation_directions_without_language_identifiers() {
        let schema = schema::parse(
            "fixture",
            br#"
tables:
  - id: 1
    name: people
    columns:
      - { id: 1, name: id, type: string, pk: true }
  - id: 2
    name: messages
    columns:
      - { id: 1, name: id, type: string, pk: true }
      - { id: 2, name: author_id, type: string }
    foreign_keys:
      - name: messages_author_id_fk
        columns: [author_id]
        ref_table: people
        ref_columns: [id]
"#,
        )
        .unwrap()
        .canonical();
        let model = Model::from_schema(&schema).unwrap();
        let people = model
            .tables
            .iter()
            .find(|table| table.name == "people")
            .unwrap();
        let messages = model
            .tables
            .iter()
            .find(|table| table.name == "messages")
            .unwrap();
        assert_eq!(
            messages.forward_relations[0].pairs,
            [("id".into(), "author_id".into())]
        );
        assert_eq!(
            people.reverse_relations[0].pairs,
            [("author_id".into(), "id".into())]
        );
    }
}
