use super::{Column, Index, Table};
use crate::engine::catalog::identity::{
    AccessGeneration, ColumnId, ExistenceGeneration, IndexId, TableId, ValueGeneration,
    WriteProtocolGeneration,
};

/// Immutable compatibility fences carried by a bound physical plan. Names are
/// diagnostic only; physical identities and generations determine admission.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CatalogDependencies {
    pub table_existence: Vec<TableExistenceDependency>,
    pub column_values: Vec<ColumnValueDependency>,
    pub index_access: Vec<IndexAccessDependency>,
    pub write_protocols: Vec<WriteProtocolDependency>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TableExistenceDependency {
    pub table_id: TableId,
    pub table_name: String,
    pub generation: ExistenceGeneration,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ColumnValueDependency {
    pub table_id: TableId,
    pub table_name: String,
    pub column_id: ColumnId,
    pub column_name: String,
    pub generation: ValueGeneration,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IndexAccessDependency {
    pub table_id: TableId,
    pub table_name: String,
    pub index_id: IndexId,
    pub index_name: String,
    pub generation: AccessGeneration,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WriteProtocolDependency {
    pub table_id: TableId,
    pub table_name: String,
    pub generation: WriteProtocolGeneration,
}

impl CatalogDependencies {
    pub fn is_empty(&self) -> bool {
        self.table_existence.is_empty()
            && self.column_values.is_empty()
            && self.index_access.is_empty()
            && self.write_protocols.is_empty()
    }

    pub fn add_table_read(&mut self, table: &Table, columns: &[Column]) {
        push_unique(
            &mut self.table_existence,
            TableExistenceDependency {
                table_id: table.id.clone(),
                table_name: table.name.clone(),
                generation: table.existence_generation,
            },
            |left, right| left.table_id == right.table_id && left.generation == right.generation,
        );
        for column in columns {
            push_unique(
                &mut self.column_values,
                ColumnValueDependency {
                    table_id: table.id.clone(),
                    table_name: table.name.clone(),
                    column_id: column.id.clone(),
                    column_name: column.name.clone(),
                    generation: column.value_generation,
                },
                |left, right| {
                    left.table_id == right.table_id
                        && left.column_id == right.column_id
                        && left.generation == right.generation
                },
            );
        }
    }

    pub fn add_index_read(&mut self, table: &Table, index: &Index, columns: &[Column]) {
        self.add_table_read(table, columns);
        push_unique(
            &mut self.index_access,
            IndexAccessDependency {
                table_id: table.id.clone(),
                table_name: table.name.clone(),
                index_id: index.id.clone(),
                index_name: index.name.clone(),
                generation: index.access_generation,
            },
            |left, right| {
                left.table_id == right.table_id
                    && left.index_id == right.index_id
                    && left.generation == right.generation
            },
        );
    }

    pub fn add_table_write(&mut self, table: &Table) {
        self.add_table_read(table, &table.columns);
        push_unique(
            &mut self.write_protocols,
            WriteProtocolDependency {
                table_id: table.id.clone(),
                table_name: table.name.clone(),
                generation: table.write_protocol_generation,
            },
            |left, right| left.table_id == right.table_id && left.generation == right.generation,
        );
    }

    pub fn merge(&mut self, other: &Self) {
        for dependency in &other.table_existence {
            push_unique(
                &mut self.table_existence,
                dependency.clone(),
                |left, right| {
                    left.table_id == right.table_id && left.generation == right.generation
                },
            );
        }
        for dependency in &other.column_values {
            push_unique(
                &mut self.column_values,
                dependency.clone(),
                |left, right| {
                    left.table_id == right.table_id
                        && left.column_id == right.column_id
                        && left.generation == right.generation
                },
            );
        }
        for dependency in &other.index_access {
            push_unique(&mut self.index_access, dependency.clone(), |left, right| {
                left.table_id == right.table_id
                    && left.index_id == right.index_id
                    && left.generation == right.generation
            });
        }
        for dependency in &other.write_protocols {
            push_unique(
                &mut self.write_protocols,
                dependency.clone(),
                |left, right| {
                    left.table_id == right.table_id && left.generation == right.generation
                },
            );
        }
    }
}

fn push_unique<T>(values: &mut Vec<T>, value: T, same: impl Fn(&T, &T) -> bool) {
    if !values.iter().any(|existing| same(existing, &value)) {
        values.push(value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::catalog::identity::{DefinitionGeneration, LogicalIndexId, SchemaId};
    use crate::engine::catalog::model::{ScalarType, Table};

    fn fixture() -> (Table, Index) {
        let column = Column {
            id: "c1".into(),
            schema_id: SchemaId::new(1).unwrap(),
            name: "id".into(),
            value_generation: 4.into(),
            scalar_type: ScalarType::Text,
            nullable: false,
            format: String::new(),
            insert_default: None,
            missing_value: None,
        };
        let index = Index {
            id: "i1".into(),
            logical_id: LogicalIndexId::from("li1"),
            definition_generation: DefinitionGeneration::from(3),
            access_generation: AccessGeneration::from(5),
            name: "primary".into(),
            columns: vec!["id".into()],
            column_ids: vec!["c1".into()],
            unique: true,
            ..Index::default()
        };
        let table = Table {
            id: "t1".into(),
            schema_id: SchemaId::new(1).unwrap(),
            name: "users".into(),
            definition_generation: 2.into(),
            existence_generation: 6.into(),
            write_protocol_generation: 7.into(),
            columns: vec![column],
            primary_key: vec!["id".into()],
            indexes: vec![index.clone()],
            foreign_keys: Vec::new(),
            constraints: Vec::new(),
        };
        (table, index)
    }

    #[test]
    fn table_write_records_all_required_fence_classes_once() {
        let (table, _) = fixture();
        let mut dependencies = CatalogDependencies::default();
        dependencies.add_table_write(&table);
        dependencies.add_table_write(&table);
        assert_eq!(dependencies.table_existence.len(), 1);
        assert_eq!(dependencies.column_values.len(), 1);
        assert_eq!(dependencies.write_protocols.len(), 1);
    }

    #[test]
    fn index_read_adds_access_to_the_table_and_value_fences() {
        let (table, index) = fixture();
        let mut dependencies = CatalogDependencies::default();
        dependencies.add_index_read(&table, &index, &table.columns);
        assert_eq!(dependencies.table_existence.len(), 1);
        assert_eq!(dependencies.column_values.len(), 1);
        assert_eq!(dependencies.index_access.len(), 1);
    }
}
