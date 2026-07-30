//! Canonical row bodies and order-preserving key tuples.

use std::collections::{HashMap, HashSet};

use crate::engine::catalog::identity::ColumnId;
use crate::engine::catalog::model::{Column, ColumnConversion, DefaultValue, ScalarType, Table};
use crate::engine::kv::key_encoding;
use crate::engine::lir::{Row, Value};

use super::{Error, ErrorKind, Result};

const ROW_CANARY: u8 = b'R';

pub fn data_prefix(table: &Table) -> Vec<u8> {
    data_prefix_for(&table.id)
}

pub(crate) fn data_prefix_for(table_id: &crate::engine::catalog::identity::TableId) -> Vec<u8> {
    format!("/rad/data/{table_id}/primary/").into_bytes()
}

pub fn data_key(table: &Table, primary_key: &[u8]) -> Vec<u8> {
    let mut key = data_prefix(table);
    key.extend_from_slice(primary_key);
    key
}

pub fn index_prefix(table: &Table, index_id: &impl std::fmt::Display) -> Vec<u8> {
    index_prefix_for(&table.id, index_id)
}

pub(crate) fn index_prefix_for(
    table_id: &crate::engine::catalog::identity::TableId,
    index_id: &impl std::fmt::Display,
) -> Vec<u8> {
    format!("/rad/index/{table_id}/{index_id}/").into_bytes()
}

pub fn index_key(
    table: &Table,
    index_id: &impl std::fmt::Display,
    indexed: &[u8],
    primary_key: &[u8],
) -> Vec<u8> {
    let mut key = index_prefix(table, index_id);
    key.extend_from_slice(indexed);
    key.extend_from_slice(primary_key);
    key
}

pub fn encode_value(value: &Value) -> Result<Vec<u8>> {
    Ok(match value {
        Value::Null(_) => key_encoding::encode_null().to_vec(),
        Value::Text(value) => key_encoding::encode_text(value),
        Value::Int64(value) => key_encoding::encode_i64(*value).to_vec(),
        Value::Float64(value) => {
            if !value.is_finite() {
                return Err(Error::with_reason(
                    ErrorKind::Runtime,
                    super::ErrorReason::NumericOverflow,
                    "exec: cannot encode a non-finite float64 in a key",
                ));
            }
            key_encoding::encode_f64(if *value == 0.0 { 0.0 } else { *value })
                .expect("finite float64 has an ordered encoding")
                .to_vec()
        }
        Value::Bool(value) => key_encoding::encode_bool(*value).to_vec(),
    })
}

pub fn encode_tuple(values: &[Value]) -> Result<Vec<u8>> {
    let mut output = Vec::new();
    for value in values {
        output.extend_from_slice(&encode_value(value)?);
    }
    Ok(output)
}

pub fn encode_row_tuple(row: &Row, columns: &[String]) -> Result<Vec<u8>> {
    let values = columns
        .iter()
        .map(|column| {
            row.get(column).cloned().ok_or_else(|| {
                Error::message(
                    ErrorKind::Internal,
                    format!("exec: missing value for column {column:?}"),
                )
            })
        })
        .collect::<Result<Vec<_>>>()?;
    encode_tuple(&values)
}

pub fn decode_tuple(mut input: &[u8]) -> Result<Vec<Value>> {
    let mut values = Vec::new();
    while !input.is_empty() {
        let (value, consumed) = decode_value(input)?;
        values.push(value);
        input = &input[consumed..];
    }
    Ok(values)
}

pub fn decode_value(input: &[u8]) -> Result<(Value, usize)> {
    let Some(tag) = input.first().copied() else {
        return Err(corrupt("exec: truncated encoded value"));
    };
    match tag {
        key_encoding::TAG_NULL => Ok((Value::Null(ScalarType::Text), 1)),
        key_encoding::TAG_BOOL => match input.get(1) {
            Some(0) => Ok((Value::Bool(false), 2)),
            Some(1) => Ok((Value::Bool(true), 2)),
            _ => Err(corrupt("exec: malformed bool key value")),
        },
        key_encoding::TAG_I64 => {
            let bytes: [u8; 8] = input
                .get(1..9)
                .ok_or_else(|| corrupt("exec: truncated int64 key value"))?
                .try_into()
                .expect("slice length checked");
            Ok((
                Value::Int64((u64::from_be_bytes(bytes) ^ (1 << 63)) as i64),
                9,
            ))
        }
        key_encoding::TAG_F64 => {
            let bytes: [u8; 8] = input
                .get(1..9)
                .ok_or_else(|| corrupt("exec: truncated float64 key value"))?
                .try_into()
                .expect("slice length checked");
            let ordered = u64::from_be_bytes(bytes);
            let raw = if ordered & (1 << 63) != 0 {
                ordered & !(1 << 63)
            } else {
                !ordered
            };
            let value = f64::from_bits(raw);
            if !value.is_finite() || (value == 0.0 && value.is_sign_negative()) {
                return Err(corrupt("exec: non-canonical float64 key value"));
            }
            Ok((Value::Float64(value), 9))
        }
        key_encoding::TAG_TEXT => decode_text(input),
        _ => Err(corrupt(format!("exec: unknown key value tag 0x{tag:02x}"))),
    }
}

fn decode_text(input: &[u8]) -> Result<(Value, usize)> {
    let mut bytes = Vec::new();
    let mut position = 1;
    loop {
        let Some(byte) = input.get(position).copied() else {
            return Err(corrupt("exec: unterminated text key value"));
        };
        position += 1;
        if byte != 0 {
            bytes.push(byte);
            continue;
        }
        match input.get(position).copied() {
            Some(0xff) => {
                bytes.push(0);
                position += 1;
            }
            Some(0x01) => {
                position += 1;
                let text = String::from_utf8(bytes).map_err(|error| {
                    Error::source(
                        ErrorKind::CorruptData,
                        "exec: invalid UTF-8 key text",
                        error,
                    )
                })?;
                return Ok((Value::Text(text), position));
            }
            _ => return Err(corrupt("exec: malformed text key escape")),
        }
    }
}

pub fn marshal_row(table: &Table, row: &Row) -> Result<Vec<u8>> {
    let mut fields = row
        .iter()
        .map(|(name, value)| {
            let column = table.column(name).ok_or_else(|| {
                Error::message(
                    ErrorKind::Internal,
                    format!("codec: table {:?} has no column {name:?}", table.name),
                )
            })?;
            Ok((physical_id(column)?, value))
        })
        .collect::<Result<Vec<_>>>()?;
    fields.sort_by_key(|(id, _)| *id);

    let mut output = vec![ROW_CANARY];
    append_uvarint(&mut output, fields.len() as u64);
    let mut previous = 0;
    for (id, value) in fields {
        let delta = id
            .checked_sub(previous)
            .filter(|delta| *delta != 0)
            .ok_or_else(|| corrupt("codec: non-ascending physical column ID"))?;
        previous = id;
        let header = delta << 1;
        if value.is_null() {
            append_uvarint(&mut output, header | 1);
        } else {
            append_uvarint(&mut output, header);
            let payload = encode_payload(value)?;
            append_uvarint(&mut output, payload.len() as u64);
            output.extend_from_slice(&payload);
        }
    }
    Ok(output)
}

pub fn unmarshal_row(table: &Table, raw: &[u8]) -> Result<Row> {
    unmarshal_row_columns(table, &table.columns, raw)
}

pub fn unmarshal_row_columns(table: &Table, columns: &[Column], raw: &[u8]) -> Result<Row> {
    if raw.first() != Some(&ROW_CANARY) {
        return Err(corrupt("codec: value is not a Rad row (bad canary)"));
    }
    let live: HashSet<_> = table.columns.iter().map(|column| &column.id).collect();
    let mut requested = HashMap::new();
    for (index, column) in columns.iter().enumerate() {
        if !live.contains(&column.id) {
            return Err(corrupt(format!(
                "codec: column {:?} does not belong to table {:?}",
                column.name, table.name
            )));
        }
        let id = physical_id(column)?;
        if requested.insert(id, index).is_some() {
            return Err(corrupt(format!(
                "codec: column {:?} requested twice",
                column.name
            )));
        }
    }

    let mut position = 1;
    let count = read_uvarint(raw, &mut position)?;
    validate_field_count(count, raw.len() - position)?;
    let mut values = vec![None; columns.len()];
    let mut previous = 0_u64;
    for _ in 0..count {
        let header = read_uvarint(raw, &mut position)?;
        let delta = header >> 1;
        let id = previous
            .checked_add(delta)
            .filter(|_| delta != 0)
            .ok_or_else(|| corrupt("codec: duplicate, non-ascending, or overflowing column ID"))?;
        previous = id;
        if header & 1 == 1 {
            if let Some(index) = requested.get(&id) {
                values[*index] = Some(Value::Null(columns[*index].scalar_type));
            }
            continue;
        }
        let length = usize::try_from(read_uvarint(raw, &mut position)?)
            .map_err(|_| corrupt("codec: payload length overflows usize"))?;
        let end = position
            .checked_add(length)
            .filter(|end| *end <= raw.len())
            .ok_or_else(|| corrupt(format!("codec: truncated payload for column ID {id}")))?;
        if let Some(index) = requested.get(&id) {
            values[*index] = Some(decode_payload(
                columns[*index].scalar_type,
                &raw[position..end],
            )?);
        }
        position = end;
    }
    if position != raw.len() {
        return Err(corrupt(format!(
            "codec: {} trailing bytes after {count} fields",
            raw.len() - position
        )));
    }
    columns
        .iter()
        .enumerate()
        .map(|(index, column)| {
            Ok((
                column.name.clone(),
                values[index]
                    .clone()
                    .map_or_else(|| decode_missing_value(column), Ok)?,
            ))
        })
        .collect()
}

pub fn decode_missing_value(column: &Column) -> Result<Value> {
    let Some(default) = &column.missing_value else {
        return Ok(Value::Null(column.scalar_type));
    };
    if default.function.is_some() {
        return Err(corrupt(
            "codec: historical missing value cannot use a generator",
        ));
    }
    Ok(default_value(column.scalar_type, default))
}

/// Add or replace one immutable physical column representation while
/// preserving every unrelated stored cell.
pub fn set_column_value(raw: &[u8], column: &Column, value: &Value) -> Result<Vec<u8>> {
    if !value.is_null() && value.scalar_type() != column.scalar_type {
        return Err(Error::message(
            ErrorKind::Internal,
            format!(
                "codec: physical column {:?} expects {:?}, got {:?}",
                column.id,
                column.scalar_type,
                value.scalar_type()
            ),
        ));
    }
    let target = physical_id(column)?;
    let mut fields = decode_physical_fields(raw)?;
    let replacement = PhysicalField {
        id: target,
        is_null: value.is_null(),
        payload: if value.is_null() {
            None
        } else {
            Some(encode_payload(value)?)
        },
    };
    match fields.binary_search_by_key(&target, |field| field.id) {
        Ok(position) => fields[position] = replacement,
        Err(position) => fields.insert(position, replacement),
    }
    Ok(encode_physical_fields(&fields))
}

pub fn read_column_value(raw: &[u8], column: &Column) -> Result<Value> {
    let target = physical_id(column)?;
    let fields = decode_physical_fields(raw)?;
    let Some(field) = fields.into_iter().find(|field| field.id == target) else {
        return decode_missing_value(column);
    };
    if field.is_null {
        return Ok(Value::Null(column.scalar_type));
    }
    decode_payload(
        column.scalar_type,
        field
            .payload
            .as_deref()
            .expect("non-null physical field has a payload"),
    )
}

/// Remove one retired physical cell while preserving the sparse row framing.
pub fn remove_column(raw: &[u8], column_id: &ColumnId) -> Result<(Vec<u8>, bool)> {
    let target = parse_physical_column_id(column_id)?;
    let mut fields = decode_physical_fields(raw)?;
    let Ok(position) = fields.binary_search_by_key(&target, |field| field.id) else {
        return Ok((raw.to_vec(), false));
    };
    fields.remove(position);
    Ok((encode_physical_fields(&fields), true))
}

/// Deterministic, locale-free conversion used by online column replacement.
pub fn convert_column_value(
    value: &Value,
    target: &Column,
    conversion: ColumnConversion,
) -> Result<Value> {
    if conversion != ColumnConversion::StrictBuiltin {
        return Err(Error::message(
            ErrorKind::Internal,
            "codec: unsupported column conversion",
        ));
    }
    if value.is_null() {
        if !target.nullable {
            return Err(Error::message(
                ErrorKind::ConstraintViolation,
                format!(
                    "codec: NULL cannot be converted to non-nullable {:?}",
                    target.scalar_type
                ),
            ));
        }
        return Ok(Value::Null(target.scalar_type));
    }
    if matches!(value, Value::Float64(value) if !value.is_finite()) {
        return Err(Error::message(
            ErrorKind::ConstraintViolation,
            "codec: cannot convert a non-finite float64",
        ));
    }
    if value.scalar_type() == target.scalar_type {
        return Ok(value.clone());
    }
    let converted = match (value, target.scalar_type) {
        (Value::Int64(value), ScalarType::Text) => Value::Text(value.to_string()),
        (Value::Float64(value), ScalarType::Text) => Value::Text(value.to_string()),
        (Value::Bool(value), ScalarType::Text) => Value::Text(value.to_string()),
        (Value::Text(value), ScalarType::Int64) => {
            Value::Int64(value.parse().map_err(|error| {
                Error::source(
                    ErrorKind::ConstraintViolation,
                    format!("codec: text {value:?} is not a base-10 int64"),
                    error,
                )
            })?)
        }
        (Value::Float64(value), ScalarType::Int64)
            if value.is_finite()
                && value.trunc() == *value
                && *value >= -(2_f64.powi(63))
                && *value < 2_f64.powi(63) =>
        {
            Value::Int64(*value as i64)
        }
        (Value::Text(value), ScalarType::Float64) => {
            let parsed = value.parse::<f64>().map_err(|error| {
                Error::source(
                    ErrorKind::ConstraintViolation,
                    format!("codec: text {value:?} is not a float64"),
                    error,
                )
            })?;
            if !parsed.is_finite() {
                return Err(Error::message(
                    ErrorKind::ConstraintViolation,
                    format!("codec: text {value:?} is not a finite float64"),
                ));
            }
            Value::Float64(parsed)
        }
        (Value::Int64(value), ScalarType::Float64)
            if (*value as f64) as i128 == i128::from(*value) =>
        {
            Value::Float64(*value as f64)
        }
        (Value::Text(value), ScalarType::Bool) if value == "true" => Value::Bool(true),
        (Value::Text(value), ScalarType::Bool) if value == "false" => Value::Bool(false),
        _ => {
            return Err(Error::message(
                ErrorKind::ConstraintViolation,
                format!(
                    "codec: no strict built-in conversion from {:?} to {:?}",
                    value.scalar_type(),
                    target.scalar_type
                ),
            ));
        }
    };
    Ok(converted)
}

struct PhysicalField {
    id: u64,
    is_null: bool,
    payload: Option<Vec<u8>>,
}

fn decode_physical_fields(raw: &[u8]) -> Result<Vec<PhysicalField>> {
    if raw.first() != Some(&ROW_CANARY) {
        return Err(corrupt("codec: value is not a Rad row (bad canary)"));
    }
    let mut position = 1;
    let count = read_uvarint(raw, &mut position)?;
    validate_field_count(count, raw.len() - position)?;
    let mut fields = Vec::new();
    let mut previous = 0_u64;
    for _ in 0..count {
        let header = read_uvarint(raw, &mut position)?;
        let delta = header >> 1;
        let id = previous
            .checked_add(delta)
            .filter(|_| delta != 0)
            .ok_or_else(|| corrupt("codec: duplicate, non-ascending, or overflowing column ID"))?;
        previous = id;
        let is_null = header & 1 == 1;
        let payload = if is_null {
            None
        } else {
            let length = usize::try_from(read_uvarint(raw, &mut position)?)
                .map_err(|_| corrupt("codec: payload length overflow"))?;
            let end = position
                .checked_add(length)
                .filter(|end| *end <= raw.len())
                .ok_or_else(|| corrupt("codec: truncated physical payload"))?;
            let payload = raw[position..end].to_vec();
            position = end;
            Some(payload)
        };
        fields.push(PhysicalField {
            id,
            is_null,
            payload,
        });
    }
    if position != raw.len() {
        return Err(corrupt("codec: trailing physical row bytes"));
    }
    Ok(fields)
}

fn encode_physical_fields(fields: &[PhysicalField]) -> Vec<u8> {
    let mut output = vec![ROW_CANARY];
    append_uvarint(&mut output, fields.len() as u64);
    let mut previous = 0;
    for field in fields {
        let header = (field.id - previous) << 1;
        previous = field.id;
        append_uvarint(&mut output, header | u64::from(field.is_null));
        if let Some(payload) = &field.payload {
            append_uvarint(&mut output, payload.len() as u64);
            output.extend_from_slice(payload);
        }
    }
    output
}

fn default_value(value_type: ScalarType, default: &DefaultValue) -> Value {
    match value_type {
        ScalarType::Text => Value::Text(default.text.clone()),
        ScalarType::Int64 => Value::Int64(default.int64),
        ScalarType::Float64 => Value::Float64(default.float64),
        ScalarType::Bool => Value::Bool(default.bool_value),
    }
}

fn physical_id(column: &Column) -> Result<u64> {
    parse_physical_column_id(&column.id).map_err(|_| {
        corrupt(format!(
            "codec: column {:?} has malformed physical ID {:?}",
            column.name, column.id
        ))
    })
}

fn parse_physical_column_id(column_id: &ColumnId) -> Result<u64> {
    column_id
        .as_str()
        .strip_prefix('c')
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|value| *value > 0 && *value <= i64::MAX as u64)
        .ok_or_else(|| corrupt(format!("codec: malformed physical column ID {column_id:?}")))
}

fn encode_payload(value: &Value) -> Result<Vec<u8>> {
    Ok(match value {
        Value::Text(value) => value.as_bytes().to_vec(),
        Value::Int64(value) => value.to_be_bytes().to_vec(),
        Value::Float64(value) if value.is_finite() => value.to_bits().to_be_bytes().to_vec(),
        Value::Float64(_) => {
            return Err(Error::with_reason(
                ErrorKind::Runtime,
                super::ErrorReason::NumericOverflow,
                "exec: cannot store a non-finite float64",
            ));
        }
        Value::Bool(value) => vec![u8::from(*value)],
        Value::Null(_) => unreachable!("null payloads have no body"),
    })
}

fn decode_payload(value_type: ScalarType, payload: &[u8]) -> Result<Value> {
    match value_type {
        ScalarType::Text => String::from_utf8(payload.to_vec())
            .map(Value::Text)
            .map_err(|error| {
                Error::source(ErrorKind::CorruptData, "codec: invalid UTF-8 text", error)
            }),
        ScalarType::Int64 if payload.len() == 8 => Ok(Value::Int64(i64::from_be_bytes(
            payload.try_into().expect("length checked"),
        ))),
        ScalarType::Float64 if payload.len() == 8 => {
            let value = f64::from_bits(u64::from_be_bytes(
                payload.try_into().expect("length checked"),
            ));
            if !value.is_finite() {
                return Err(corrupt("codec: non-finite float64 payload"));
            }
            Ok(Value::Float64(value))
        }
        ScalarType::Bool if payload == [0] => Ok(Value::Bool(false)),
        ScalarType::Bool if payload == [1] => Ok(Value::Bool(true)),
        _ => Err(corrupt(format!(
            "codec: malformed {value_type:?} payload of {} bytes",
            payload.len()
        ))),
    }
}

fn append_uvarint(output: &mut Vec<u8>, mut value: u64) {
    while value >= 0x80 {
        output.push((value as u8) | 0x80);
        value >>= 7;
    }
    output.push(value as u8);
}

fn validate_field_count(count: u64, remaining: usize) -> Result<()> {
    if count > remaining as u64 {
        return Err(corrupt("codec: field count exceeds remaining row bytes"));
    }
    Ok(())
}

fn read_uvarint(input: &[u8], position: &mut usize) -> Result<u64> {
    let mut value = 0_u64;
    for shift in (0..70).step_by(7) {
        let byte = *input
            .get(*position)
            .ok_or_else(|| corrupt("codec: truncated varint"))?;
        *position += 1;
        if shift == 63 && byte > 1 {
            return Err(corrupt("codec: overflowing varint"));
        }
        value |= u64::from(byte & 0x7f) << shift;
        if byte < 0x80 {
            if shift != 0 && byte == 0 {
                return Err(corrupt("codec: non-canonical varint"));
            }
            return Ok(value);
        }
    }
    Err(corrupt("codec: overflowing varint"))
}

fn corrupt(message: impl Into<String>) -> Error {
    Error::message(ErrorKind::CorruptData, message)
}

#[cfg(test)]
mod tests {
    use crate::engine::catalog::identity::{
        DefinitionGeneration, ExistenceGeneration, SchemaId, ValueGeneration,
        WriteProtocolGeneration,
    };
    use crate::engine::catalog::model::DefaultValue;

    use super::*;

    fn column(id: &str, schema_id: u32, name: &str, scalar_type: ScalarType) -> Column {
        Column {
            id: id.into(),
            schema_id: SchemaId::new(schema_id).unwrap(),
            name: name.into(),
            value_generation: ValueGeneration::ZERO,
            scalar_type,
            nullable: true,
            format: String::new(),
            insert_default: None,
            missing_value: None,
        }
    }

    fn table(columns: Vec<Column>) -> Table {
        Table {
            id: "t1".into(),
            schema_id: SchemaId::new(1).unwrap(),
            name: "fixture".into(),
            definition_generation: DefinitionGeneration::ZERO,
            existence_generation: ExistenceGeneration::ZERO,
            write_protocol_generation: WriteProtocolGeneration::ZERO,
            columns,
            primary_key: vec!["id".into()],
            indexes: Vec::new(),
            foreign_keys: Vec::new(),
            constraints: Vec::new(),
        }
    }

    #[test]
    fn row_bytes_are_schema_directed_and_match_the_canonical_layout() {
        let table = table(vec![
            column("c1", 1, "id", ScalarType::Text),
            column("c2", 2, "count", ScalarType::Int64),
            column("c4", 4, "ready", ScalarType::Bool),
        ]);
        let row = Row::from([
            ("id".into(), Value::Text("a".into())),
            ("count".into(), Value::Int64(7)),
            ("ready".into(), Value::Bool(true)),
        ]);
        let encoded = marshal_row(&table, &row).unwrap();
        assert_eq!(
            encoded,
            [b'R', 3, 2, 1, b'a', 2, 8, 0, 0, 0, 0, 0, 0, 0, 7, 4, 1, 1,]
        );
        assert_eq!(unmarshal_row(&table, &encoded).unwrap(), row);
    }

    #[test]
    fn absent_and_explicit_null_remain_distinct() {
        let old = table(vec![column("c1", 1, "id", ScalarType::Text)]);
        let raw = marshal_row(&old, &Row::from([("id".into(), Value::Text("a".into()))])).unwrap();
        let mut status = column("c2", 2, "status", ScalarType::Text);
        status.missing_value = Some(DefaultValue {
            text: "active".into(),
            ..DefaultValue::default()
        });
        let current = table(vec![old.columns[0].clone(), status]);
        assert_eq!(
            unmarshal_row(&current, &raw).unwrap()["status"],
            Value::Text("active".into())
        );

        let explicit = marshal_row(
            &current,
            &Row::from([
                ("id".into(), Value::Text("a".into())),
                ("status".into(), Value::Null(ScalarType::Text)),
            ]),
        )
        .unwrap();
        assert_eq!(
            unmarshal_row(&current, &explicit).unwrap()["status"],
            Value::Null(ScalarType::Text)
        );
    }

    #[test]
    fn rows_round_trip_every_scalar_boundary_and_skip_unrequested_cells() {
        let table = table(vec![
            column("c1", 1, "text", ScalarType::Text),
            column("c2", 2, "integer", ScalarType::Int64),
            column("c3", 3, "float", ScalarType::Float64),
            column("c4", 4, "boolean", ScalarType::Bool),
        ]);
        let rows = [
            Row::from([
                ("text".into(), Value::Text(String::new())),
                ("integer".into(), Value::Int64(i64::MIN)),
                ("float".into(), Value::Float64(-f64::MAX)),
                ("boolean".into(), Value::Bool(false)),
            ]),
            Row::from([
                ("text".into(), Value::Text("café — 日本語 🦀".into())),
                ("integer".into(), Value::Int64(i64::MAX)),
                ("float".into(), Value::Float64(f64::MAX)),
                ("boolean".into(), Value::Bool(true)),
            ]),
        ];
        for row in rows {
            let raw = marshal_row(&table, &row).unwrap();
            assert_eq!(unmarshal_row(&table, &raw).unwrap(), row);
            assert_eq!(
                unmarshal_row_columns(&table, &table.columns[1..2], &raw).unwrap(),
                Row::from([("integer".into(), row["integer"].clone())])
            );
        }
    }

    #[test]
    fn row_codec_rejects_non_finite_values() {
        let table = table(vec![column("c4", 4, "score", ScalarType::Float64)]);
        for value in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            let row = Row::from([("score".into(), Value::Float64(value))]);
            assert!(marshal_row(&table, &row).is_err());

            let mut raw = vec![ROW_CANARY, 1, 8, 8];
            raw.extend_from_slice(&value.to_bits().to_be_bytes());
            assert_eq!(
                unmarshal_row(&table, &raw).unwrap_err().kind(),
                ErrorKind::CorruptData
            );
        }
    }

    #[test]
    fn tuple_codec_round_trips_ordered_primitives_and_rejects_non_finite_floats() {
        let values = vec![
            Value::Null(ScalarType::Int64),
            Value::Bool(true),
            Value::Int64(i64::MIN),
            Value::Float64(-1.5),
            Value::Text("a\0b".into()),
        ];
        let encoded = encode_tuple(&values).unwrap();
        let decoded = decode_tuple(&encoded).unwrap();
        assert!(decoded[0].is_null());
        assert_eq!(&decoded[1..], &values[1..]);
        for value in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert!(encode_value(&Value::Float64(value)).is_err());
        }
        assert!(encode_value(&Value::Int64(-1)).unwrap() < encode_value(&Value::Int64(0)).unwrap());
        assert_eq!(
            encode_value(&Value::Float64(-0.0)).unwrap(),
            encode_value(&Value::Float64(0.0)).unwrap(),
            "semantic key identity must agree with numeric equality"
        );
    }

    #[test]
    fn semantic_order_and_key_bytes_agree_for_generated_scalars() {
        fn assert_key_order(left: &Value, right: &Value) {
            let semantic = left.compare(right).expect("generated types match");
            let physical = encode_value(left)
                .unwrap()
                .cmp(&encode_value(right).unwrap());
            assert_eq!(
                physical, semantic,
                "key order disagrees for {left:?} and {right:?}"
            );
        }

        fn assert_key_round_trip(value: &Value) {
            let encoded = encode_value(value).unwrap();
            let (decoded, consumed) = decode_value(&encoded).unwrap();
            assert_eq!(consumed, encoded.len(), "partial decode for {value:?}");
            assert_eq!(
                encode_value(&decoded).unwrap(),
                encoded,
                "non-canonical round trip for {value:?}"
            );
            match (value, decoded) {
                (Value::Null(_), Value::Null(_)) => {}
                (Value::Float64(expected), Value::Float64(actual)) => assert_eq!(
                    actual.to_bits(),
                    if *expected == 0.0 { 0.0 } else { *expected }.to_bits(),
                    "float key round trip for {value:?}"
                ),
                (expected, actual) => assert_eq!(&actual, expected),
            }
        }

        fn splitmix64(state: &mut u64) -> u64 {
            *state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
            let mut value = *state;
            value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
            value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
            value ^ (value >> 31)
        }

        let mut state = 0x7261_642d_6b65_7973;
        for _ in 0..4_096 {
            let left = Value::Int64(splitmix64(&mut state) as i64);
            let right = Value::Int64(splitmix64(&mut state) as i64);
            assert_key_order(&left, &right);
            assert_key_round_trip(&left);
            assert_key_round_trip(&right);

            let left = Value::Float64(f64::from_bits(splitmix64(&mut state)));
            let right = Value::Float64(f64::from_bits(splitmix64(&mut state)));
            if !matches!((&left, &right), (Value::Float64(left), Value::Float64(right)) if !left.is_finite() || !right.is_finite())
            {
                assert_key_order(&left, &right);
                assert_key_round_trip(&left);
                assert_key_round_trip(&right);
            }
        }

        for values in [
            vec![
                Value::Text(String::new()),
                Value::Text("\0".into()),
                Value::Text("a".into()),
                Value::Text("a\0b".into()),
                Value::Text("café — 日本語 🦀".into()),
            ],
            vec![Value::Bool(false), Value::Bool(true)],
            vec![
                Value::Float64(-f64::MAX),
                Value::Float64(-0.0),
                Value::Float64(0.0),
                Value::Float64(f64::MIN_POSITIVE),
                Value::Float64(f64::MAX),
            ],
        ] {
            for left in &values {
                assert_key_round_trip(left);
                for right in &values {
                    assert_key_order(left, right);
                }
            }
        }

        for scalar_type in [
            ScalarType::Text,
            ScalarType::Int64,
            ScalarType::Float64,
            ScalarType::Bool,
        ] {
            let null = Value::Null(scalar_type);
            assert_key_round_trip(&null);
            assert_key_order(&null, &Value::Null(ScalarType::Text));
            assert_key_order(&null, &Value::Text(String::new()));
        }
    }

    #[test]
    fn tuple_values_are_self_delimiting_with_trailing_values() {
        let values = vec![
            Value::Text("a\0b".into()),
            Value::Int64(7),
            Value::Bool(false),
        ];
        let encoded = encode_tuple(&values).unwrap();

        let (first, consumed) = decode_value(&encoded).unwrap();
        assert_eq!(first, values[0]);
        assert!(consumed < encoded.len());
        assert_eq!(decode_tuple(&encoded[consumed..]).unwrap(), values[1..]);
    }

    #[test]
    fn tuple_decoder_rejects_every_malformed_primitive_shape() {
        let mut nan = vec![key_encoding::TAG_F64];
        nan.extend_from_slice(&(f64::NAN.to_bits() | (1_u64 << 63)).to_be_bytes());
        let malformed = [
            vec![],
            vec![key_encoding::TAG_BOOL],
            vec![key_encoding::TAG_BOOL, 2],
            vec![key_encoding::TAG_I64, 1],
            vec![key_encoding::TAG_F64, 0],
            key_encoding::encode_f64(-0.0).unwrap().to_vec(),
            nan,
            vec![key_encoding::TAG_TEXT, b'a'],
            vec![key_encoding::TAG_TEXT, 0],
            vec![key_encoding::TAG_TEXT, 0, 2],
            vec![0xff],
        ];
        for raw in malformed {
            assert!(
                decode_value(&raw).is_err(),
                "decoded malformed bytes {raw:?}"
            );
        }
    }

    #[test]
    fn sparse_column_removal_preserves_every_other_frame() {
        let wide = table(vec![
            column("c1", 1, "first", ScalarType::Text),
            column("c5", 2, "middle", ScalarType::Int64),
            column("c99", 3, "last", ScalarType::Text),
        ]);
        let rows = [
            Row::from([
                ("first".into(), Value::Text("a".into())),
                ("middle".into(), Value::Int64(7)),
                ("last".into(), Value::Text("z".into())),
            ]),
            Row::from([
                ("first".into(), Value::Text("a".into())),
                ("middle".into(), Value::Null(ScalarType::Int64)),
                ("last".into(), Value::Text("z".into())),
            ]),
        ];
        for row in rows {
            let raw = marshal_row(&wide, &row).unwrap();
            for removed in &wide.columns {
                let (cleaned, changed) = remove_column(&raw, &removed.id).unwrap();
                assert!(changed);
                let narrow = table(
                    wide.columns
                        .iter()
                        .filter(|column| column.id != removed.id)
                        .cloned()
                        .collect(),
                );
                let decoded = unmarshal_row(&narrow, &cleaned).unwrap();
                for column in &narrow.columns {
                    assert_eq!(decoded[&column.name], row[&column.name]);
                }
            }

            let (unchanged, changed) = remove_column(&raw, &ColumnId::from("c2")).unwrap();
            assert!(!changed);
            assert_eq!(unchanged, raw);
        }
    }

    #[test]
    fn column_removal_rejects_malformed_identifiers_and_rows() {
        let cases = [
            (ColumnId::from("wat"), vec![ROW_CANARY, 0]),
            (ColumnId::from("c0"), vec![ROW_CANARY, 0]),
            (ColumnId::from("c1"), vec![0, 0]),
            (ColumnId::from("c1"), vec![ROW_CANARY, 0x80]),
            (ColumnId::from("c1"), vec![ROW_CANARY, 1, 0]),
            (ColumnId::from("c1"), vec![ROW_CANARY, 1, 2, 5, b'x']),
            (ColumnId::from("c1"), vec![ROW_CANARY, 0, 1]),
        ];
        for (column, raw) in cases {
            assert!(
                remove_column(&raw, &column).is_err(),
                "accepted column {column:?} with bytes {raw:?}"
            );
        }
    }

    #[test]
    fn strict_conversion_rejects_loss_and_locale_guessing() {
        let target = |scalar_type, nullable| {
            let mut column = column("c1", 1, "value", scalar_type);
            column.nullable = nullable;
            column
        };
        let int = target(ScalarType::Int64, false);
        let float = target(ScalarType::Float64, false);
        let boolean = target(ScalarType::Bool, false);
        let text = target(ScalarType::Text, true);

        for (input, target, expected) in [
            (Value::Text("-42".into()), &int, Value::Int64(-42)),
            (Value::Float64(42.0), &int, Value::Int64(42)),
            (
                Value::Int64(1_i64 << 53),
                &float,
                Value::Float64((1_i64 << 53) as f64),
            ),
            (Value::Text("true".into()), &boolean, Value::Bool(true)),
            (
                Value::Null(ScalarType::Int64),
                &text,
                Value::Null(ScalarType::Text),
            ),
        ] {
            assert_eq!(
                convert_column_value(&input, target, ColumnConversion::StrictBuiltin).unwrap(),
                expected
            );
        }

        for (input, target) in [
            (Value::Text("1,000".into()), &int),
            (Value::Text("NaN".into()), &float),
            (Value::Text("inf".into()), &float),
            (Value::Float64(f64::NAN), &float),
            (Value::Float64(1.5), &int),
            (Value::Float64(f64::NAN), &int),
            (Value::Int64((1_i64 << 53) + 1), &float),
            (Value::Text("TRUE".into()), &boolean),
            (Value::Null(ScalarType::Text), &int),
            (Value::Bool(true), &int),
        ] {
            assert!(
                convert_column_value(&input, target, ColumnConversion::StrictBuiltin).is_err(),
                "accepted lossy conversion {input:?} to {:?}",
                target.scalar_type
            );
        }
    }

    #[test]
    fn corrupt_frames_are_rejected_strictly() {
        let table = table(vec![column("c1", 1, "id", ScalarType::Text)]);
        for raw in [vec![], vec![b'X'], vec![b'R', 1, 2, 4, b'a']] {
            assert_eq!(
                unmarshal_row(&table, &raw).unwrap_err().kind(),
                ErrorKind::CorruptData
            );
        }
        let mut trailing =
            marshal_row(&table, &Row::from([("id".into(), Value::Text("a".into()))])).unwrap();
        trailing.push(0xff);
        assert!(unmarshal_row(&table, &trailing).is_err());
    }

    #[test]
    fn corrupt_field_count_cannot_request_unbounded_allocation() {
        let table = table(vec![column("c1", 1, "id", ScalarType::Text)]);
        let mut raw = vec![ROW_CANARY];
        append_uvarint(&mut raw, u64::MAX);

        let error = read_column_value(&raw, &table.columns[0]).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::CorruptData);
    }

    #[test]
    fn non_canonical_row_varints_are_rejected() {
        let table = table(vec![column("c1", 1, "id", ScalarType::Text)]);

        let error = unmarshal_row(&table, &[ROW_CANARY, 0x80, 0x00]).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::CorruptData);
    }

    #[test]
    fn duplicate_physical_column_ids_are_rejected() {
        let table = table(vec![
            column("c1", 1, "id", ScalarType::Text),
            column("c1", 2, "alias", ScalarType::Text),
        ]);
        let row = Row::from([
            ("id".into(), Value::Text("a".into())),
            ("alias".into(), Value::Text("b".into())),
        ]);

        assert_eq!(
            marshal_row(&table, &row).unwrap_err().kind(),
            ErrorKind::CorruptData
        );
    }

    #[test]
    fn replacement_cells_preserve_other_physical_fields() {
        let table = table(vec![
            column("c1", 1, "id", ScalarType::Text),
            column("c3", 3, "value", ScalarType::Text),
        ]);
        let row = Row::from([
            ("id".into(), Value::Text("a".into())),
            ("value".into(), Value::Text("42".into())),
        ]);
        let raw = marshal_row(&table, &row).unwrap();
        let target = column("c2", 2, "value", ScalarType::Int64);
        let converted =
            convert_column_value(&row["value"], &target, ColumnConversion::StrictBuiltin).unwrap();
        let replaced = set_column_value(&raw, &target, &converted).unwrap();

        assert_eq!(
            read_column_value(&replaced, &target).unwrap(),
            Value::Int64(42)
        );
        assert_eq!(unmarshal_row(&table, &replaced).unwrap(), row);
    }
}
