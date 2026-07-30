use rad::engine::catalog::identity::SchemaId;
use rad::engine::catalog::model::{
    ColumnDef, ForeignKeyDef, IndexDef, ScalarType, Schema, TableDef,
};

use super::Choices;

const TYPES: [ScalarType; 4] = [
    ScalarType::Text,
    ScalarType::Int64,
    ScalarType::Float64,
    ScalarType::Bool,
];

pub fn generate(choices: &mut Choices<'_>) -> Schema {
    let table_count = choices.range(1, 4);
    let mut tables = Vec::with_capacity(table_count);
    for table_index in 0..table_count {
        let table_name = format!("t{table_index}");
        let value_columns = choices.range(1, 5);
        let mut columns = vec![column(1, "id", ScalarType::Text, false)];
        for column_index in 0..value_columns {
            columns.push(column(
                (column_index + 2) as u32,
                &format!("c{column_index}"),
                TYPES[choices.index(TYPES.len())],
                choices.coin(),
            ));
        }

        let mut foreign_keys = Vec::new();
        if table_index > 0 && choices.coin() {
            let parent = choices.index(table_index);
            columns.push(column(
                (columns.len() + 1) as u32,
                "fk",
                ScalarType::Text,
                true,
            ));
            foreign_keys.push(ForeignKeyDef {
                name: format!("{table_name}_fk0"),
                columns: vec!["fk".into()],
                ref_table: format!("t{parent}"),
                ref_columns: vec!["id".into()],
            });
        }

        let mut indexes = Vec::new();
        if choices.coin() {
            let column_index = choices.range(1, value_columns);
            indexes.push(IndexDef {
                name: format!("{table_name}_i0"),
                columns: vec![format!("c{}", column_index - 1)],
                unique: false,
            });
        }
        tables.push(TableDef {
            id: id((table_index + 1) as u32),
            name: table_name,
            columns,
            primary_key: vec!["id".into()],
            indexes,
            foreign_keys,
        });
    }
    Schema::from_definitions(tables)
}

fn column(id_value: u32, name: &str, scalar_type: ScalarType, nullable: bool) -> ColumnDef {
    ColumnDef {
        id: id(id_value),
        name: name.into(),
        scalar_type,
        nullable,
        format: String::new(),
        default: None,
    }
}

fn id(value: u32) -> SchemaId {
    SchemaId::new(value).expect("generated schema IDs are non-zero")
}
