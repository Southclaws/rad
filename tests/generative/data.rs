use std::collections::BTreeMap;

use rad::engine::catalog::model::{ScalarType, Schema};
use rad::engine::lir::{Row, Value};

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
                    scalar(column.scalar_type, column.nullable, choices)
                };
                row.insert(column.name.clone(), value);
            }
            rows.push(row);
        }
        output.insert(table.name.clone(), rows);
    }
    output
}

fn scalar(scalar_type: ScalarType, nullable: bool, choices: &mut Choices<'_>) -> Value {
    if nullable && choices.chance(4) {
        return Value::Null(scalar_type);
    }
    match scalar_type {
        ScalarType::Text => Value::Text(["", "a", "b", "c"][choices.index(4)].into()),
        ScalarType::Int64 => {
            Value::Int64([i64::MIN, -2, -1, 0, 1, 2, 100, i64::MAX][choices.index(8)])
        }
        ScalarType::Float64 => Value::Float64([-1.5, -0.0, 0.0, 1.5, 2.5][choices.index(5)]),
        ScalarType::Bool => Value::Bool(choices.coin()),
    }
}
