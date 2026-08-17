//! The permanent allocation registry.
//!
//! Durable identifiers (the root magic, keyspace tags, scalar case tags,
//! record magics) and the golden vectors that pin byte semantics are
//! append-only: once released, their meanings are immutable. The registry is
//! persisted beside the specification and every compile verifies the current
//! specification against it, so an incompatible edit fails before it can
//! reach a generated artifact. Structured-value schemas are frozen per
//! version through a flattened structural signature per property path: any
//! structural change to a released schema requires a new schema version.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use sha2::{Digest, Sha256};

use crate::model::*;

#[derive(Debug, Default, PartialEq)]
pub struct Allocations {
    pub root: Option<Vec<u8>>,
    /// tag -> space name
    pub spaces: BTreeMap<u8, String>,
    /// tag -> reservation reason
    pub reserved: BTreeMap<u8, String>,
    /// "codec.case" -> tag
    pub scalar_tags: BTreeMap<String, u8>,
    /// record name -> magic bytes
    pub record_magic: BTreeMap<String, Vec<u8>>,
    /// declaration name -> digests of its golden vectors
    pub vectors: BTreeMap<String, BTreeSet<String>>,
    /// schema reference -> property path -> structural signature
    pub schemas: BTreeMap<String, BTreeMap<String, String>>,
}

/// Flattened structural view of one storage JSON Schema: property path to
/// signature (`<type> required|optional[ enum:a|b]`).
pub type SchemaShape = BTreeMap<String, String>;

pub fn from_spec(spec: &Spec, schemas: &BTreeMap<String, SchemaShape>) -> Allocations {
    let mut allocations = Allocations::default();
    for keyspace in &spec.keyspaces {
        allocations.root = Some(keyspace.magic.clone());
        for space in &keyspace.spaces {
            allocations.spaces.insert(space.tag, space.name.clone());
            for vector in &space.vectors {
                allocations
                    .vectors
                    .entry(space.name.clone())
                    .or_default()
                    .insert(key_vector_digest(vector));
            }
        }
        for (tag, reason) in &keyspace.reserved {
            allocations.reserved.insert(*tag, reason.clone());
        }
        if let Some(scan) = &keyspace.scan {
            for bound in &scan.bounds {
                allocations
                    .vectors
                    .entry("scan".to_owned())
                    .or_default()
                    .insert(vector_digest(bound));
            }
        }
    }
    for codec in &spec.codecs {
        for case in &codec.cases {
            if let Some(tag) = case.tag {
                allocations
                    .scalar_tags
                    .insert(format!("{}.{}", codec.name, case.name), tag);
            }
        }
        for vector in &codec.vectors {
            allocations
                .vectors
                .entry(codec.name.clone())
                .or_default()
                .insert(vector_digest(vector));
        }
    }
    for record in &spec.records {
        for item in &record.items {
            if let RecordItem::Magic(magic) = item {
                allocations
                    .record_magic
                    .insert(record.name.clone(), magic.clone());
            }
        }
        for vector in &record.vectors {
            allocations
                .vectors
                .entry(record.name.clone())
                .or_default()
                .insert(vector_digest(vector));
        }
    }
    allocations.schemas = schemas
        .iter()
        .map(|(reference, shape)| (reference.clone(), shape.clone()))
        .collect();
    allocations
}

fn digest(content: &str) -> String {
    let hash = Sha256::digest(content.as_bytes());
    let mut hex = String::with_capacity(16);
    for byte in &hash[..8] {
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

fn hex_bytes(bytes: &[u8]) -> String {
    let mut hex = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

/// Digest over the semantic content of a vector: notes and prose are free to
/// change, bytes and values are not.
fn vector_digest(vector: &Vector) -> String {
    match vector {
        Vector::Accept { bytes, value, .. } => {
            digest(&format!("accept {} {value:?}", hex_bytes(bytes)))
        }
        Vector::Reject { bytes, .. } => digest(&format!("reject {}", hex_bytes(bytes))),
        Vector::Order { values } => digest(&format!("order {values:?}")),
        Vector::Bound { prefix, end } => digest(&format!(
            "bound {} {:?}",
            hex_bytes(prefix),
            end.as_ref().map(|end| hex_bytes(end))
        )),
    }
}

fn key_vector_digest(vector: &KeyVector) -> String {
    digest(&format!(
        "key {:?} {}",
        vector.fields,
        hex_bytes(&vector.bytes)
    ))
}

pub fn parse(text: &str) -> Result<Allocations, String> {
    let mut allocations = Allocations::default();
    for (index, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let error = |message: &str| format!("allocations line {}: {message}", index + 1);
        let mut parts = line.splitn(2, ' ');
        let keyword = parts.next().unwrap_or_default();
        let rest = parts.next().unwrap_or_default();
        match keyword {
            "root" => {
                allocations.root =
                    Some(parse_hex(rest).ok_or_else(|| error("malformed root magic"))?);
            }
            "space" | "reserved" => {
                let (tag, name) = rest
                    .split_once(' ')
                    .ok_or_else(|| error("expected `<tag> <name>`"))?;
                let tag = u8::from_str_radix(tag, 16).map_err(|_| error("malformed tag byte"))?;
                if keyword == "space" {
                    allocations.spaces.insert(tag, name.to_owned());
                } else {
                    allocations.reserved.insert(tag, name.to_owned());
                }
            }
            "scalar" => {
                let (name, tag) = rest
                    .rsplit_once(' ')
                    .ok_or_else(|| error("expected `<codec.case> <tag>`"))?;
                let tag = u8::from_str_radix(tag, 16).map_err(|_| error("malformed scalar tag"))?;
                allocations.scalar_tags.insert(name.to_owned(), tag);
            }
            "record" => {
                let (name, magic) = rest
                    .split_once(' ')
                    .ok_or_else(|| error("expected `<record> <magic>`"))?;
                let magic = parse_hex(magic).ok_or_else(|| error("malformed record magic"))?;
                allocations.record_magic.insert(name.to_owned(), magic);
            }
            "vector" => {
                let (name, digest) = rest
                    .split_once(' ')
                    .ok_or_else(|| error("expected `<declaration> <digest>`"))?;
                allocations
                    .vectors
                    .entry(name.to_owned())
                    .or_default()
                    .insert(digest.to_owned());
            }
            "schema" => {
                let mut parts = rest.splitn(3, ' ');
                let (Some(reference), Some(path), Some(signature)) =
                    (parts.next(), parts.next(), parts.next())
                else {
                    return Err(error("expected `<reference> <path> <signature>`"));
                };
                allocations
                    .schemas
                    .entry(reference.to_owned())
                    .or_default()
                    .insert(path.to_owned(), signature.to_owned());
            }
            other => return Err(error(&format!("unknown allocation kind {other:?}"))),
        }
    }
    Ok(allocations)
}

fn parse_hex(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        return None;
    }
    text.as_bytes()
        .chunks_exact(2)
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok())
        .collect()
}

pub fn render(allocations: &Allocations) -> String {
    let mut out = String::new();
    out.push_str("# Permanent durable-format allocations, maintained by keyform.\n");
    out.push_str("# Never edit by hand. Allocated meanings are immutable: a removed\n");
    out.push_str("# keyspace becomes a `reserved` line forever, released vectors are\n");
    out.push_str("# normative, and released schema versions are structurally frozen.\n");
    if let Some(root) = &allocations.root {
        let _ = writeln!(out, "root {}", hex_bytes(root));
    }
    for (tag, name) in &allocations.spaces {
        let _ = writeln!(out, "space {tag:02x} {name}");
    }
    for (tag, reason) in &allocations.reserved {
        let _ = writeln!(out, "reserved {tag:02x} {reason}");
    }
    for (name, tag) in &allocations.scalar_tags {
        let _ = writeln!(out, "scalar {name} {tag:02x}");
    }
    for (name, magic) in &allocations.record_magic {
        let _ = writeln!(out, "record {name} {}", hex_bytes(magic));
    }
    for (name, digests) in &allocations.vectors {
        for digest in digests {
            let _ = writeln!(out, "vector {name} {digest}");
        }
    }
    for (reference, shape) in &allocations.schemas {
        for (path, signature) in shape {
            let _ = writeln!(out, "schema {reference} {path} {signature}");
        }
    }
    out
}

/// Every released allocation must survive into the new registry with its
/// meaning intact. Returns the full list of violations.
pub fn verify_compatible(old: &Allocations, new: &Allocations) -> Vec<String> {
    let mut errors = Vec::new();
    if let (Some(old_root), Some(new_root)) = (&old.root, &new.root)
        && old_root != new_root
    {
        errors.push(format!(
            "root magic changed from {} to {}: the root magic is permanent",
            hex_bytes(old_root),
            hex_bytes(new_root)
        ));
    }
    for (tag, name) in &old.spaces {
        match (new.spaces.get(tag), new.reserved.contains_key(tag)) {
            (Some(new_name), _) if new_name == name => {}
            (Some(new_name), _) => errors.push(format!(
                "keyspace tag {tag:02x} changed meaning from {name} to {new_name}: allocated tags are permanent"
            )),
            (None, true) => {}
            (None, false) => errors.push(format!(
                "keyspace tag {tag:02x} ({name}) was removed without reservation: removed tags stay reserved forever"
            )),
        }
        if let Some((new_tag, _)) = new
            .spaces
            .iter()
            .find(|(new_tag, new_name)| *new_name == name && *new_tag != tag)
        {
            errors.push(format!(
                "space {name} moved from tag {tag:02x} to {new_tag:02x}: reallocate under a new name instead"
            ));
        }
    }
    for tag in old.reserved.keys() {
        if new.spaces.contains_key(tag) {
            errors.push(format!(
                "reserved tag {tag:02x} was reallocated: reserved tags are never reused"
            ));
        }
        if !new.reserved.contains_key(tag) && !new.spaces.contains_key(tag) {
            errors.push(format!(
                "reserved tag {tag:02x} disappeared from the registry"
            ));
        }
    }
    for (name, tag) in &old.scalar_tags {
        match new.scalar_tags.get(name) {
            Some(new_tag) if new_tag == tag => {}
            Some(new_tag) => errors.push(format!(
                "scalar tag {name} changed from {tag:02x} to {new_tag:02x}: scalar tags are permanent"
            )),
            None => errors.push(format!("scalar tag {name} was removed: scalar tags are permanent")),
        }
    }
    for (name, magic) in &old.record_magic {
        match new.record_magic.get(name) {
            Some(new_magic) if new_magic == magic => {}
            Some(new_magic) => errors.push(format!(
                "record {name} magic changed from {} to {}",
                hex_bytes(magic),
                hex_bytes(new_magic)
            )),
            None => errors.push(format!("record {name} was removed from the registry")),
        }
    }
    for (name, digests) in &old.vectors {
        let Some(new_digests) = new.vectors.get(name) else {
            errors.push(format!(
                "all golden vectors for {name} were removed: released vectors are normative"
            ));
            continue;
        };
        for digest in digests {
            if !new_digests.contains(digest) {
                errors.push(format!(
                    "a golden vector for {name} changed or was removed ({digest}): released vectors are normative"
                ));
            }
        }
    }
    for (reference, shape) in &old.schemas {
        let Some(new_shape) = new.schemas.get(reference) else {
            errors.push(format!(
                "storage schema {reference} was removed: released schemas are permanent"
            ));
            continue;
        };
        for (path, signature) in shape {
            match new_shape.get(path) {
                Some(new_signature) if new_signature == signature => {}
                Some(new_signature) => errors.push(format!(
                    "storage schema {reference} changed {path} from `{signature}` to `{new_signature}`: released schema versions are frozen; add a new schema version"
                )),
                None => errors.push(format!(
                    "storage schema {reference} removed property {path}: released schema versions are frozen; add a new schema version"
                )),
            }
        }
        for path in new_shape.keys() {
            if !shape.contains_key(path) {
                errors.push(format!(
                    "storage schema {reference} adds property {path}: released schema versions are frozen; add a new schema version"
                ));
            }
        }
    }
    errors
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> Allocations {
        let mut allocations = Allocations {
            root: Some(vec![0x72, 0x37]),
            ..Allocations::default()
        };
        allocations.spaces.insert(0x01, "data".into());
        allocations.reserved.insert(0x00, "zero guard".into());
        allocations
            .scalar_tags
            .insert("ordered_scalar.null".into(), 0x01);
        allocations
            .record_magic
            .insert("row_body".into(), vec![0x52]);
        allocations
            .vectors
            .entry("uvarint".into())
            .or_default()
            .insert("aaaa".into());
        allocations
            .schemas
            .entry("catalog/table.v1".into())
            .or_default()
            .extend([
                ("id".to_owned(), "string required".to_owned()),
                ("state".to_owned(), "string optional enum:a|b".to_owned()),
            ]);
        allocations
    }

    #[test]
    fn from_spec_collects_allocations_with_distinct_digests() {
        let spec = crate::parse(
            r#"format storage v0
            codec uvarint {
              vectors {
                accept x"00" = int 0
                accept x"01" = int 1
                reject x"80 00" "non-canonical"
              }
            }
            record row_body { magic x"52" }
            keyspace k {
              magic x"72 37"
              reserved x"00" "zero guard"
              space a {
                tag x"01"
                key { g uvarint }
                value "v"
                vectors {
                  key { g 0 } = x"72 37 01 00"
                  key { g 1 } = x"72 37 01 01"
                }
              }
            }
            "#,
        )
        .expect("spec parses");
        let allocations = from_spec(&spec, &BTreeMap::new());
        assert_eq!(allocations.root, Some(vec![0x72, 0x37]));
        assert_eq!(allocations.spaces.get(&0x01), Some(&"a".to_owned()));
        assert_eq!(
            allocations.reserved.get(&0x00),
            Some(&"zero guard".to_owned())
        );
        assert_eq!(allocations.record_magic.get("row_body"), Some(&vec![0x52]));
        assert_eq!(allocations.vectors["uvarint"].len(), 3);
        assert_eq!(allocations.vectors["a"].len(), 2);
    }

    #[test]
    fn round_trips_through_text() {
        let allocations = base();
        let parsed = parse(&render(&allocations)).expect("rendered registry parses");
        assert_eq!(parsed, allocations);
    }

    #[test]
    fn compatible_additions_pass() {
        let old = base();
        let mut new = base();
        new.spaces.insert(0x02, "index".into());
        new.vectors
            .get_mut("uvarint")
            .unwrap()
            .insert("bbbb".into());
        new.schemas
            .entry("catalog/table.v2".into())
            .or_default()
            .insert("id".into(), "string required".into());
        assert!(verify_compatible(&old, &new).is_empty());
    }

    #[test]
    fn incompatible_changes_are_rejected() {
        let old = base();

        let mut renamed = base();
        renamed.spaces.insert(0x01, "rows".into());
        let mut removed = base();
        removed.spaces.clear();
        let mut retagged = base();
        retagged.spaces.clear();
        retagged.spaces.insert(0x03, "data".into());
        retagged.reserved.insert(0x01, "data".into());
        let mut new_root = base();
        new_root.root = Some(vec![0x99, 0x99]);
        let mut magic_changed = base();
        magic_changed
            .record_magic
            .insert("row_body".into(), vec![0x99]);
        let mut reserved_gone = base();
        reserved_gone.reserved.clear();
        let mut reserved_reused = base();
        reserved_reused.reserved.clear();
        reserved_reused.spaces.insert(0x00, "sneaky".into());
        let mut vector_gone = base();
        vector_gone.vectors.get_mut("uvarint").unwrap().clear();
        let mut scalar_change = base();
        scalar_change
            .scalar_tags
            .insert("ordered_scalar.null".into(), 0x09);
        let mut property_gone = base();
        property_gone
            .schemas
            .get_mut("catalog/table.v1")
            .unwrap()
            .remove("id");
        let mut type_change = base();
        type_change
            .schemas
            .get_mut("catalog/table.v1")
            .unwrap()
            .insert("id".into(), "integer required".into());
        let mut property_added = base();
        property_added
            .schemas
            .get_mut("catalog/table.v1")
            .unwrap()
            .insert("note".into(), "string optional".into());
        let mut enum_shrunk = base();
        enum_shrunk
            .schemas
            .get_mut("catalog/table.v1")
            .unwrap()
            .insert("state".into(), "string optional enum:a".into());
        let mut enum_grown = base();
        enum_grown
            .schemas
            .get_mut("catalog/table.v1")
            .unwrap()
            .insert("state".into(), "string optional enum:a|b|c".into());
        let mut now_optional = base();
        now_optional
            .schemas
            .get_mut("catalog/table.v1")
            .unwrap()
            .insert("id".into(), "string optional".into());

        for (case, new, needle) in [
            ("rename", renamed, "changed meaning"),
            ("removal", removed, "without reservation"),
            ("retag", retagged, "moved from tag"),
            ("root", new_root, "root magic changed"),
            ("record magic", magic_changed, "magic changed"),
            ("reserved gone", reserved_gone, "disappeared"),
            ("reserved reused", reserved_reused, "reallocated"),
            ("vector", vector_gone, "vector for uvarint"),
            ("scalar", scalar_change, "scalar tag"),
            ("property gone", property_gone, "removed property"),
            ("type", type_change, "changed id"),
            ("property added", property_added, "adds property"),
            ("enum shrunk", enum_shrunk, "changed state"),
            ("enum grown", enum_grown, "changed state"),
            ("now optional", now_optional, "changed id"),
        ] {
            let errors = verify_compatible(&old, &new);
            assert!(
                errors.iter().any(|error| error.contains(needle)),
                "{case}: expected {needle:?} in {errors:?}"
            );
        }
    }
}
