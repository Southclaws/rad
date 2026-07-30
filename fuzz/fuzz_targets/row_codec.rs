#![no_main]

use libfuzzer_sys::fuzz_target;
use rad::engine::catalog::identity::{
    DefinitionGeneration, ExistenceGeneration, SchemaId, ValueGeneration, WriteProtocolGeneration,
};
use rad::engine::catalog::model::{Column, ScalarType, Table};
use rad::engine::exec::codec::{
    marshal_row, read_column_value, remove_column, set_column_value, unmarshal_row,
};
use rad::engine::lir::Value;

fuzz_target!(|input: &[u8]| {
    if input.len() > 64 * 1024 {
        return;
    }
    let decoded;
    let input = if let Some(hex) = input.strip_prefix(b"hex:") {
        let Some(bytes) = decode_hex(hex) else {
            return;
        };
        decoded = bytes;
        decoded.as_slice()
    } else {
        input
    };
    let table = table();

    if let Ok(row) = unmarshal_row(&table, input) {
        let encoded = marshal_row(&table, &row).expect("decoded rows can be re-encoded");
        assert_eq!(
            unmarshal_row(&table, &encoded).expect("re-encoded row decodes"),
            row
        );
    }

    for (column, value) in table.columns.iter().zip([
        Value::Text("fuzz\0text".into()),
        Value::Int64(i64::MIN),
        Value::Bool(true),
        Value::Float64(-0.0),
    ]) {
        let _ = read_column_value(input, column);
        for value in [value, Value::Null(column.scalar_type)] {
            let Ok(replaced) = set_column_value(input, column, &value) else {
                continue;
            };
            let actual =
                read_column_value(&replaced, column).expect("replacement is readable");
            assert_eq!(actual, value);
            if let (Value::Float64(actual), Value::Float64(expected)) = (&actual, &value) {
                assert_eq!(actual.to_bits(), expected.to_bits());
            }
            assert_eq!(
                set_column_value(&replaced, column, &value).expect("replacement is repeatable"),
                replaced
            );

            let (removed, existed) =
                remove_column(&replaced, &column.id).expect("replacement can be removed");
            assert!(existed);
            let (removed_again, existed_again) =
                remove_column(&removed, &column.id).expect("removal is repeatable");
            assert!(!existed_again);
            assert_eq!(removed_again, removed);
        }
    }
});

fn table() -> Table {
    Table {
        id: "t1".into(),
        schema_id: schema_id(1),
        name: "fuzz_rows".into(),
        definition_generation: DefinitionGeneration::ZERO,
        existence_generation: ExistenceGeneration::ZERO,
        write_protocol_generation: WriteProtocolGeneration::ZERO,
        columns: vec![
            column("c1", 1, "id", ScalarType::Text),
            column("c2", 2, "count", ScalarType::Int64),
            column("c3", 3, "ready", ScalarType::Bool),
            column("c4", 4, "score", ScalarType::Float64),
        ],
        primary_key: vec!["id".into()],
        indexes: Vec::new(),
        foreign_keys: Vec::new(),
        constraints: Vec::new(),
    }
}

fn column(id: &str, schema_id_value: u32, name: &str, scalar_type: ScalarType) -> Column {
    Column {
        id: id.into(),
        schema_id: schema_id(schema_id_value),
        name: name.into(),
        value_generation: ValueGeneration::ZERO,
        scalar_type,
        nullable: true,
        format: String::new(),
        insert_default: None,
        missing_value: None,
    }
}

fn schema_id(value: u32) -> SchemaId {
    SchemaId::new(value).expect("fuzz schema IDs are positive")
}

fn decode_hex(input: &[u8]) -> Option<Vec<u8>> {
    let input = input.strip_suffix(b"\n").unwrap_or(input);
    let input = input.strip_suffix(b"\r").unwrap_or(input);
    if input.len() % 2 != 0 {
        return None;
    }
    input
        .chunks_exact(2)
        .map(|pair| Some((digit(pair[0])? << 4) | digit(pair[1])?))
        .collect()
}

fn digit(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}
