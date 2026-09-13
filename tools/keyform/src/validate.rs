//! Structural validation running between parse and emit.
//!
//! The emitter panics on malformed input; the validator turns every such
//! condition into an authored-spec error with a message naming the
//! declaration, so specification mistakes never surface as compiler panics.

use std::collections::HashSet;

use crate::model::*;

const SCALARS: [&str; 5] = ["text", "int64", "float64", "bool", "bytes"];
const CODEC_LAWS: [&str; 5] = [
    "round_trip",
    "canonical_bytes",
    "self_delimiting",
    "order_preserving",
    "concatenation",
];
const RECORD_LAWS: [&str; 2] = ["round_trip", "cell_ops"];
const ENCODE_CONVENTIONS: [&str; 3] = ["append", "value_bytes", "slice_bytes"];
const DECODE_CONVENTIONS: [&str; 3] = ["positional", "consumed", "whole"];

pub fn validate(spec: &Spec) -> Result<(), Vec<String>> {
    let mut errors = Vec::new();

    let mut names = HashSet::new();
    for name in spec
        .codecs
        .iter()
        .map(|codec| &codec.name)
        .chain(spec.records.iter().map(|record| &record.name))
        .chain(spec.fixtures.iter().map(|fixture| &fixture.name))
        .chain(spec.domains.iter().map(|domain| &domain.name))
    {
        if !names.insert(name.clone()) {
            errors.push(format!("duplicate declaration name {name:?}"));
        }
    }

    for codec in &spec.codecs {
        validate_codec(spec, codec, &mut errors);
    }
    for record in &spec.records {
        validate_record(spec, record, &mut errors);
    }
    for fixture in &spec.fixtures {
        validate_fixture(fixture, &mut errors);
    }
    // The emitters render loop-unsafe items (registry constants, the spaces
    // table) once per keyspace, so a specification holds at most one.
    if spec.keyspaces.len() > 1 {
        errors.push("a specification declares at most one keyspace".to_owned());
    }
    for keyspace in &spec.keyspaces {
        validate_keyspace(keyspace, &mut errors);
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

fn validate_codec(spec: &Spec, codec: &Codec, errors: &mut Vec<String>) {
    let name = &codec.name;
    if let Some(target) = &codec.refines
        && spec.codec(target).is_none()
    {
        errors.push(format!("codec {name} refines unknown codec {target:?}"));
    }
    for law in &codec.laws {
        if !CODEC_LAWS.contains(&law.name.as_str()) {
            errors.push(format!("codec {name} declares unknown law {:?}", law.name));
        }
    }
    for kernel in &codec.kernels {
        let allowed: &[&str] = match kernel.role.as_str() {
            "encode" => &ENCODE_CONVENTIONS,
            "decode" => &DECODE_CONVENTIONS,
            "module" | "element_decode" => &[],
            other => {
                errors.push(format!("codec {name} has unknown kernel role {other:?}"));
                continue;
            }
        };
        if let Some(convention) = &kernel.convention
            && !allowed.contains(&convention.as_str())
        {
            errors.push(format!(
                "codec {name} kernel {} has unknown convention {convention:?}",
                kernel.role
            ));
        }
        if kernel.path.is_empty() {
            errors.push(format!("codec {name} kernel {} has no path", kernel.role));
        }
    }

    let has_decode = codec.kernel("decode").is_some();
    let tagged = codec.kernel("module").is_some();
    let mut tags = HashSet::new();
    for case in &codec.cases {
        if let Some(tag) = case.tag
            && !tags.insert(tag)
        {
            errors.push(format!(
                "codec {name} case {} reuses tag 0x{tag:02x}",
                case.name
            ));
        }
        if tagged && case.encoder.is_none() {
            errors.push(format!(
                "codec {name} case {} needs an encoder binding",
                case.name
            ));
        }
        if !SCALARS.contains(&case.name.as_str()) && case.name != "null" {
            errors.push(format!("codec {name} has unknown case {:?}", case.name));
        }
    }
    for vector in &codec.vectors {
        match vector {
            Vector::Reject { .. } if !has_decode => {
                errors.push(format!(
                    "codec {name} has reject vectors but no decode kernel"
                ));
            }
            Vector::Bound { .. } => {
                errors.push(format!("codec {name} carries a scan bound vector"));
            }
            _ => {}
        }
    }
    if codec.law("concatenation").is_some() && codec.kernel("element_decode").is_none() {
        errors.push(format!(
            "codec {name} declares concatenation but binds no element_decode kernel"
        ));
    }
    if codec.law("order_preserving").is_some() {
        let comparator = codec
            .law("order_preserving")
            .and_then(|law| law.comparator.as_deref());
        if !matches!(comparator, Some("semantic" | "per_type_total")) {
            errors.push(format!(
                "codec {name} order_preserving law needs comparator semantic or per_type_total"
            ));
        }
    }
}

fn validate_record(spec: &Spec, record: &Record, errors: &mut Vec<String>) {
    let name = &record.name;
    for law in &record.laws {
        if !RECORD_LAWS.contains(&law.name.as_str()) {
            errors.push(format!("record {name} declares unknown law {:?}", law.name));
        }
    }
    let needs_fixture = !record.laws.is_empty() || !record.vectors.is_empty();
    match &record.fixture {
        Some(fixture) if spec.fixture(fixture).is_none() => {
            errors.push(format!("record {name} binds unknown fixture {fixture:?}"));
        }
        None if needs_fixture => {
            errors.push(format!("record {name} has laws or vectors but no fixture"));
        }
        _ => {}
    }
    for role in ["encode", "decode"] {
        if needs_fixture && record.kernel(role).is_none() {
            errors.push(format!("record {name} binds no {role} kernel"));
        }
    }
    if record.law("cell_ops").is_some() {
        for role in ["set", "read", "remove"] {
            if record.kernel(role).is_none() {
                errors.push(format!("record {name} cell_ops law binds no {role} kernel"));
            }
        }
    }
    for vector in &record.vectors {
        match vector {
            Vector::Accept { value, .. } if !matches!(value, SpecValue::Row(_)) => {
                errors.push(format!("record {name} accept vectors carry row values"));
            }
            Vector::Order { .. } | Vector::Bound { .. } => {
                errors.push(format!("record {name} vectors are accept/reject only"));
            }
            _ => {}
        }
    }
    if let Some(fixture_name) = &record.fixture
        && let Some(fixture) = spec.fixture(fixture_name)
    {
        for vector in &record.vectors {
            let Vector::Accept {
                value: SpecValue::Row(fields),
                ..
            } = vector
            else {
                continue;
            };
            for (field, _) in fields {
                if !fixture
                    .columns
                    .iter()
                    .any(|column| column.column_name == *field)
                {
                    errors.push(format!(
                        "record {name} vector names unknown fixture column {field:?}"
                    ));
                }
            }
        }
    }
}

fn validate_fixture(fixture: &Fixture, errors: &mut Vec<String>) {
    let name = &fixture.name;
    if fixture.columns.is_empty() {
        errors.push(format!("fixture {name} has no columns"));
    }
    let mut ids = HashSet::new();
    let mut schema_ids = HashSet::new();
    let mut column_names = HashSet::new();
    for column in &fixture.columns {
        if !ids.insert(&column.id) {
            errors.push(format!("fixture {name} reuses column id {:?}", column.id));
        }
        if !schema_ids.insert(column.schema_id) {
            errors.push(format!(
                "fixture {name} reuses schema id {}",
                column.schema_id
            ));
        }
        if !column_names.insert(&column.column_name) {
            errors.push(format!(
                "fixture {name} reuses column name {:?}",
                column.column_name
            ));
        }
        if !SCALARS.contains(&column.scalar.as_str()) {
            errors.push(format!(
                "fixture {name} column {} has unknown scalar {:?}",
                column.id, column.scalar
            ));
        }
    }
}

fn validate_keyspace(keyspace: &Keyspace, errors: &mut Vec<String>) {
    let name = &keyspace.name;
    if keyspace.magic.is_empty() {
        errors.push(format!("keyspace {name} declares no root magic"));
    }
    let mut tags: HashSet<u8> = HashSet::new();
    let mut names: HashSet<&str> = HashSet::new();
    for (tag, _) in &keyspace.reserved {
        if !tags.insert(*tag) {
            errors.push(format!("tag {tag:02x} is reserved more than once"));
        }
    }
    for space in &keyspace.spaces {
        let space_name = &space.name;
        if !tags.insert(space.tag) {
            errors.push(format!(
                "space {space_name} tag {:02x} collides with another allocation",
                space.tag
            ));
        }
        if !names.insert(space_name) {
            errors.push(format!("duplicate space name {space_name}"));
        }
        if !space
            .name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        {
            errors.push(format!("space {space_name} name must be lower_snake_case"));
        }
        if matches!(&space.value, ValueForm::Opaque(text) if text.is_empty()) {
            errors.push(format!("space {space_name} declares no value format"));
        }
        validate_key_fields(space, errors);
        for vector in &space.vectors {
            validate_key_vector(space, vector, errors);
        }
    }
}

/// Unframed segments are restricted to the terminal run of a key. Tuple
/// elements self-delimit, so a tuple may be followed only by more tuples:
/// the combined suffix decodes as one scalar stream. Raw tails (text/bytes)
/// carry no framing at all, so a raw tail must be the last field and must
/// not follow a tuple: the boundary between them is undecodable.
fn validate_key_fields(space: &Space, errors: &mut Vec<String>) {
    let space_name = &space.name;
    let mut field_names = HashSet::new();
    let mut after_tuple = false;
    let mut after_raw_tail = false;
    for (index, field) in space.fields.iter().enumerate() {
        if !field_names.insert(&field.name) {
            errors.push(format!(
                "space {space_name} reuses key field name {:?}",
                field.name
            ));
        }
        if after_raw_tail {
            errors.push(format!(
                "space {space_name} field {} follows the terminal run",
                field.name
            ));
        }
        match &field.kind {
            SegmentKind::Tuple => after_tuple = true,
            SegmentKind::Text | SegmentKind::Bytes => {
                if after_tuple {
                    errors.push(format!(
                        "space {space_name} field {} is a raw tail after a tuple",
                        field.name
                    ));
                }
                after_raw_tail = true;
                if index + 1 != space.fields.len() {
                    errors.push(format!(
                        "space {space_name} field {} is a raw tail but is not last",
                        field.name
                    ));
                }
            }
            _ => {
                if after_tuple {
                    errors.push(format!(
                        "space {space_name} field {} follows the terminal run",
                        field.name
                    ));
                }
            }
        }
    }
}

fn validate_key_vector(space: &Space, vector: &KeyVector, errors: &mut Vec<String>) {
    let space_name = &space.name;
    for field in &space.fields {
        let Some((_, value)) = vector.fields.iter().find(|(name, _)| *name == field.name) else {
            errors.push(format!(
                "space {space_name} vector lacks field {:?}",
                field.name
            ));
            continue;
        };
        let matches = matches!(
            (&field.kind, value),
            (
                SegmentKind::PhysicalId(_)
                    | SegmentKind::Uvarint
                    | SegmentKind::Be32
                    | SegmentKind::Be64,
                KeyFieldValue::Number(_)
            ) | (
                SegmentKind::Tuple | SegmentKind::LenBytes | SegmentKind::Bytes,
                KeyFieldValue::Bytes(_)
            ) | (SegmentKind::Text, KeyFieldValue::Text(_))
        );
        if !matches {
            errors.push(format!(
                "space {space_name} vector field {:?} has the wrong value shape",
                field.name
            ));
        }
    }
    for (name, _) in &vector.fields {
        if !space.fields.iter().any(|field| field.name == *name) {
            errors.push(format!(
                "space {space_name} vector names unknown field {name:?}"
            ));
        }
    }
    for field in &space.fields {
        if !matches!(field.kind, SegmentKind::Be32) {
            continue;
        }
        if let Some((name, KeyFieldValue::Number(number))) =
            vector.fields.iter().find(|(name, _)| *name == field.name)
            && u32::try_from(*number).is_err()
        {
            errors.push(format!(
                "space {space_name} vector field {name:?} overflows be32"
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::parse;

    #[test]
    fn rejects_incoherent_specifications() {
        for (input, needle) in [
            (
                "format s v0 codec a {} codec a {}",
                "duplicate declaration name",
            ),
            ("format s v0 codec a { law wobble }", "unknown law"),
            (
                r#"format s v0 codec a { kernel encode "p" convention whole }"#,
                "unknown convention",
            ),
            (
                r#"format s v0 codec a { vectors { reject x"00" "no decoder" } }"#,
                "no decode kernel",
            ),
            (
                r#"format s v0 fixture table f { column c1 1 "id" text column c1 2 "b" text }"#,
                "reuses column id",
            ),
            (
                r#"format s v0 record r { law cell_ops fixture f } fixture table f { column c1 1 "id" text }"#,
                "binds no set kernel",
            ),
            (
                r#"format s v0 keyspace k { magic x"72 37" space a { tag x"01" value "v" } space b { tag x"01" value "v" } }"#,
                "collides",
            ),
            (
                r#"format s v0 keyspace k { magic x"72 37" reserved x"01" "gone" space a { tag x"01" value "v" } }"#,
                "collides",
            ),
            (
                r#"format s v0 keyspace k { magic x"72 37" space a { tag x"01" key { t tuple g uvarint } value "v" } }"#,
                "follows the terminal run",
            ),
            (
                r#"format s v0 keyspace k { magic x"72 37" space a { tag x"01" key { t text g uvarint } value "v" } }"#,
                "raw tail but is not last",
            ),
            (
                r#"format s v0 keyspace k { magic x"72 37" space a { tag x"01" key { t tuple s text } value "v" } }"#,
                "raw tail after a tuple",
            ),
            (
                r#"format s v0 keyspace k { magic x"72 37" space a { tag x"01" key { t tuple b len_bytes } value "v" } }"#,
                "follows the terminal run",
            ),
            (
                r#"format s v0 keyspace k { magic x"72 37" } keyspace j { magic x"72 37" }"#,
                "at most one keyspace",
            ),
            (
                r#"format s v0 keyspace k { magic x"72 37" space a { tag x"01" key { g uvarint n be32 } value "v" vectors { key { n 4294967296, g 1 } = x"72 37 01 01 ff ff ff ff" } } }"#,
                "overflows be32",
            ),
            (
                r#"format s v0 keyspace k { magic x"72 37" space a { tag x"01" key { g uvarint } value "v" vectors { key { g x"00" } = x"72 37 01 00" } } }"#,
                "wrong value shape",
            ),
            (
                r#"format s v0 keyspace k { magic x"72 37" space a { tag x"01" key { g uvarint } value "v" vectors { key { } = x"72 37 01 00" } } }"#,
                "vector lacks field",
            ),
            (
                "format s v0 codec a { law order_preserving }",
                "needs comparator",
            ),
        ] {
            let spec = parse(input).expect(input);
            let errors = validate(&spec).expect_err(input);
            assert!(
                errors.iter().any(|error| error.contains(needle)),
                "{input:?} produced {errors:?}"
            );
        }
    }
}
