use std::collections::BTreeMap;

use rad::engine::catalog::model::{ScalarType, Schema};
use rad::engine::lir::{BytesValue, Row, Value};

use super::Choices;

pub fn generate(schema: &Schema, choices: &mut Choices<'_>) -> BTreeMap<String, Vec<Row>> {
    let mut output = BTreeMap::<String, Vec<Row>>::new();
    for table in &schema.tables {
        // Keep the required smoke campaign bounded. Larger cardinalities and
        // combinatorial join pressure belong in the resource-budget/soak arm.
        let row_count = choices.range(0, 3);
        let mut rows = Vec::with_capacity(row_count);
        for row_index in 0..row_count {
            let mut row = Row::new();
            for column in &table.columns {
                let value = if column.name == "id" {
                    Value::Text(format!("k{row_index}"))
                } else if column.name == "fk" {
                    let foreign_key = &table.foreign_keys[0];
                    let parents = output
                        .get(&foreign_key.ref_table)
                        .expect("parents are generated before children");
                    if parents.is_empty() || choices.chance(3) {
                        Value::Null(ScalarType::Text)
                    } else {
                        parents[choices.index(parents.len())]["id"].clone()
                    }
                } else {
                    scalar(column, choices)
                };
                row.insert(column.name.clone(), value);
            }
            rows.push(row);
        }
        output.insert(table.name.clone(), rows);
    }
    output
}

fn scalar(column: &rad::engine::catalog::model::ColumnDef, choices: &mut Choices<'_>) -> Value {
    if column.nullable && choices.chance(4) {
        return Value::Null(column.scalar_type);
    }
    match column.scalar_type {
        ScalarType::Text => Value::Text(["", "a", "b", "c"][choices.index(4)].into()),
        ScalarType::Int64 => {
            Value::Int64([i64::MIN, -2, -1, 0, 1, 2, 100, i64::MAX][choices.index(8)])
        }
        ScalarType::Float64 => Value::Float64([-1.5, -0.0, 0.0, 1.5, 2.5][choices.index(5)]),
        ScalarType::Bool => Value::Bool(choices.coin()),
        ScalarType::Bytes => {
            let length = match column.format.as_str() {
                "uuid" | "ulid" => 16,
                "xid" => 12,
                _ => [0, 1, 2, 3][choices.index(4)],
            };
            Value::Bytes(BytesValue::raw(
                (0..length)
                    .map(|index| (choices.index(256) ^ index) as u8)
                    .collect::<Vec<_>>(),
            ))
        }
    }
}
