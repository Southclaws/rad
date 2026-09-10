//! Deterministic structural identity for LIR.
//!
//! Fingerprints hash a canonical byte encoding of the bound tree, never wire
//! JSON. Caller-chosen node IDs, scope labels, binding names, and binder slot
//! numbering are normalized out; tables enter as stable logical `SchemaId`s,
//! so identity survives renames. Every relation subtree receives two digests
//! in one bottom-up pass: **exact** (literal values encoded into the hashed
//! byte stream) and **family** (every literal replaced by a typed
//! placeholder). Fingerprints are persistent keys, so every digest carries
//! the canonicalization version and hash-algorithm identifier that produced
//! it: a stored digest is never reinterpreted under encoding rules that did
//! not make it.

use std::collections::{BTreeSet, HashMap};
use std::fmt;

use sha2::{Digest as _, Sha256};

use crate::engine::catalog::identity::SchemaId;

use super::bound::{self, Expr, Relation, RelationNode};
use super::{
    AggregateFunction, BinaryOp, JoinKind, Kind, RecursiveAccumulation, RootCardinality,
    SetQuantifier, SlotId, TextComparison, UnaryOp, Value,
};

pub const CANONICALIZATION_VERSION: u8 = 1;
pub const HASH_SHA256_128: u8 = 1;

const DOMAIN_EXACT: u8 = 0xE1;
const DOMAIN_FAMILY: u8 = 0xF1;
const DOMAIN_REQUEST: u8 = 0xA1;
pub(crate) const DOMAIN_PLAN: u8 = 0xB1;
const DOMAIN_EXPRESSIONS: u8 = 0xC1;

const TAG_SCAN: u8 = 1;
const TAG_ROWS: u8 = 2;
const TAG_FILTER: u8 = 3;
const TAG_PROJECT: u8 = 4;
const TAG_JOIN: u8 = 5;
const TAG_CONCATENATE: u8 = 6;
const TAG_INTERSECT: u8 = 7;
const TAG_EXCEPT: u8 = 8;
const TAG_AGGREGATE: u8 = 9;
const TAG_ORDER: u8 = 10;
const TAG_SLICE: u8 = 11;
const TAG_REF: u8 = 12;
const TAG_BACKREF: u8 = 13;
const TAG_DISTINCT: u8 = 14;
const TAG_BINDING: u8 = 15;
const TAG_QUERY: u8 = 16;
const TAG_EXTERNAL_REF: u8 = 17;

const TAG_EXPR_LITERAL: u8 = 32;
const TAG_EXPR_SLOT: u8 = 33;
const TAG_EXPR_UNARY: u8 = 34;
const TAG_EXPR_BINARY: u8 = 35;
const TAG_EXPR_CAST: u8 = 36;
const TAG_EXPR_BRANCH: u8 = 37;
const TAG_EXPR_TEXT_MATCH: u8 = 38;
const TAG_EXPR_EXISTS: u8 = 39;
const TAG_EXPR_FIRST: u8 = 40;
const TAG_EXPR_SCALAR: u8 = 41;
const TAG_EXPR_ARRAY: u8 = 42;

const VALUE_TEXT: u8 = 1;
const VALUE_INT64: u8 = 2;
const VALUE_FLOAT64: u8 = 3;
const VALUE_BOOL: u8 = 4;
const VALUE_NULL: u8 = 5;
const VALUE_PLACEHOLDER: u8 = 6;

#[derive(Clone, Copy, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct Fingerprint {
    pub canonicalization_version: u8,
    pub hash_algorithm: u8,
    pub digest: [u8; 16],
}

impl fmt::Display for Fingerprint {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "c{}h{}:",
            self.canonicalization_version, self.hash_algorithm
        )?;
        for byte in self.digest {
            write!(formatter, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl fmt::Debug for Fingerprint {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, formatter)
    }
}

impl Fingerprint {
    /// The persisted 18-byte form: canonicalization version, hash algorithm,
    /// digest. Keys in the statistics keyspace use exactly these bytes.
    pub fn to_bytes(self) -> [u8; 18] {
        let mut bytes = [0u8; 18];
        bytes[0] = self.canonicalization_version;
        bytes[1] = self.hash_algorithm;
        bytes[2..].copy_from_slice(&self.digest);
        bytes
    }

    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        let bytes: &[u8; 18] = bytes.try_into().ok()?;
        let mut digest = [0u8; 16];
        digest.copy_from_slice(&bytes[2..]);
        Some(Self {
            canonicalization_version: bytes[0],
            hash_algorithm: bytes[1],
            digest,
        })
    }
}

impl serde::Serialize for Fingerprint {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> serde::Deserialize<'de> for Fingerprint {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = <String as serde::Deserialize>::deserialize(deserializer)?;
        let (prefix, hex) = text
            .split_once(':')
            .ok_or_else(|| serde::de::Error::custom("fingerprint missing ':'"))?;
        let (version, algorithm) = prefix
            .strip_prefix('c')
            .and_then(|rest| rest.split_once('h'))
            .ok_or_else(|| serde::de::Error::custom("fingerprint prefix"))?;
        let mut digest = [0u8; 16];
        if hex.len() != 32 {
            return Err(serde::de::Error::custom("fingerprint digest length"));
        }
        for (index, byte) in digest.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&hex[index * 2..index * 2 + 2], 16)
                .map_err(serde::de::Error::custom)?;
        }
        Ok(Self {
            canonicalization_version: version.parse().map_err(serde::de::Error::custom)?,
            hash_algorithm: algorithm.parse().map_err(serde::de::Error::custom)?,
            digest,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct RelationFingerprints {
    pub exact: Fingerprint,
    pub family: Fingerprint,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QueryFingerprints {
    pub exact: Fingerprint,
    pub family: Fingerprint,
    pub root: RelationFingerprints,
    /// Every relation subtree in canonical traversal order, including
    /// relations nested inside crossing expressions and binding definitions.
    pub subtrees: Vec<RelationFingerprints>,
    /// Logical dependency set: stable schema identities of scanned tables.
    pub tables: BTreeSet<SchemaId>,
    /// Binding name to the digests of the relation it defines. The digests
    /// are the definition's own, so a relation materialized as a binding
    /// here is recognised when it appears inline in another query.
    pub bindings: Vec<(String, RelationFingerprints)>,
}

pub fn query(query: &bound::Query) -> QueryFingerprints {
    let mut canonicalizer = Canonicalizer::new(&query.bindings);
    let root = canonicalizer.relation(&query.root);

    let mut payload = Pair::default();
    payload.byte(TAG_QUERY);
    payload.byte(cardinality_byte(query.cardinality));
    payload.digests(root);
    payload.u64(canonicalizer.order.len() as u64);
    for position in canonicalizer.order.clone() {
        let digests = canonicalizer.finished[&position];
        payload.digests(digests);
    }

    QueryFingerprints {
        exact: finish(DOMAIN_EXACT, &payload.exact),
        family: finish(DOMAIN_FAMILY, &payload.family),
        root,
        subtrees: canonicalizer.subtrees,
        tables: canonicalizer.tables,
        bindings: canonicalizer.binding_roots,
    }
}

/// Exact identity of an unbound query before catalog access.
///
/// This identity retains names, scopes, literal text, and declared types. It
/// can select cached binding work only for an equal request. Binding-map order
/// is not part of query meaning, so bindings use lexical name order.
pub fn request(query: &super::unbound::Query) -> Fingerprint {
    request_with_size(query).0
}

pub(crate) fn request_with_size(query: &super::unbound::Query) -> (Fingerprint, usize) {
    let mut payload = RequestWriter::default();
    payload.byte(TAG_QUERY);
    payload.byte(cardinality_byte(query.cardinality));
    request_relation(&mut payload, &query.root);
    let mut bindings = query.bindings.iter().collect::<Vec<_>>();
    bindings.sort_unstable_by(|left, right| left.0.cmp(right.0));
    payload.len(bindings.len());
    for (name, relation) in bindings {
        payload.str(name);
        request_relation(&mut payload, relation);
    }
    let encoded_bytes = payload.bytes.len();
    (finish(DOMAIN_REQUEST, &payload.bytes), encoded_bytes)
}

#[derive(Default)]
struct RequestWriter {
    bytes: Vec<u8>,
}

impl RequestWriter {
    fn byte(&mut self, value: u8) {
        self.bytes.push(value);
    }

    fn len(&mut self, value: usize) {
        self.u64(value as u64);
    }

    fn u64(&mut self, value: u64) {
        self.bytes.extend_from_slice(&value.to_be_bytes());
    }

    fn str(&mut self, value: &str) {
        self.len(value.len());
        self.bytes.extend_from_slice(value.as_bytes());
    }

    fn optional_kind(&mut self, value: Option<Kind>) {
        match value {
            Some(value) => {
                self.byte(1);
                self.byte(kind_byte(value));
            }
            None => self.byte(0),
        }
    }
}

fn request_relation(payload: &mut RequestWriter, relation: &super::unbound::Relation) {
    use super::unbound::Relation;

    match relation {
        Relation::Scan { table, scope } => {
            payload.byte(TAG_SCAN);
            payload.str(table);
            payload.str(scope);
        }
        Relation::Rows {
            scope,
            columns,
            values,
        } => {
            payload.byte(TAG_ROWS);
            payload.str(scope);
            payload.len(columns.len());
            for column in columns {
                payload.str(&column.name);
                payload.byte(kind_byte(column.kind));
                payload.byte(u8::from(column.nullable));
            }
            payload.len(values.len());
            for row in values {
                payload.len(row.len());
                for value in row {
                    request_raw_scalar(payload, value);
                }
            }
        }
        Relation::Filter { input, predicate } => {
            payload.byte(TAG_FILTER);
            request_relation(payload, input);
            request_expr(payload, predicate);
        }
        Relation::Project {
            input,
            scope,
            spread,
            fields,
        } => {
            payload.byte(TAG_PROJECT);
            request_relation(payload, input);
            request_optional_str(payload, scope.as_deref());
            payload.len(spread.len());
            for scope in spread {
                payload.str(scope);
            }
            payload.len(fields.len());
            for field in fields {
                payload.str(&field.name);
                request_expr(payload, &field.expression);
            }
        }
        Relation::Join {
            left,
            right,
            kind,
            on,
        } => {
            payload.byte(TAG_JOIN);
            payload.byte(join_byte(*kind));
            request_relation(payload, left);
            request_relation(payload, right);
            request_expr(payload, on);
        }
        Relation::Concatenate { scope, inputs } => {
            payload.byte(TAG_CONCATENATE);
            payload.str(scope);
            payload.len(inputs.len());
            for input in inputs {
                request_relation(payload, input);
            }
        }
        Relation::Intersect {
            scope,
            left,
            right,
            quantifier,
        } => {
            payload.byte(TAG_INTERSECT);
            payload.str(scope);
            payload.byte(quantifier_byte(*quantifier));
            request_relation(payload, left);
            request_relation(payload, right);
        }
        Relation::Except {
            scope,
            left,
            right,
            quantifier,
        } => {
            payload.byte(TAG_EXCEPT);
            payload.str(scope);
            payload.byte(quantifier_byte(*quantifier));
            request_relation(payload, left);
            request_relation(payload, right);
        }
        Relation::Aggregate {
            input,
            scope,
            groups,
            terms,
        } => {
            payload.byte(TAG_AGGREGATE);
            request_relation(payload, input);
            request_optional_str(payload, scope.as_deref());
            payload.len(groups.len());
            for group in groups {
                payload.str(&group.name);
                request_expr(payload, &group.expression);
            }
            payload.len(terms.len());
            for term in terms {
                payload.byte(aggregate_byte(term.function));
                match &term.argument {
                    Some(argument) => {
                        payload.byte(1);
                        request_expr(payload, argument);
                    }
                    None => payload.byte(0),
                }
                payload.str(&term.name);
            }
        }
        Relation::Order { input, terms } => {
            payload.byte(TAG_ORDER);
            request_relation(payload, input);
            payload.len(terms.len());
            for term in terms {
                request_expr(payload, &term.expression);
                payload.byte(u8::from(term.descending));
            }
        }
        Relation::Slice {
            input,
            offset,
            limit,
        } => {
            payload.byte(TAG_SLICE);
            request_relation(payload, input);
            payload.u64(*offset as u64);
            match limit {
                Some(limit) => {
                    payload.byte(1);
                    payload.u64(*limit as u64);
                }
                None => payload.byte(0),
            }
        }
        Relation::Ref { binding, scope } => {
            payload.byte(TAG_REF);
            payload.str(binding);
            payload.str(scope);
        }
        Relation::RecursiveRef { binding, scope } => {
            payload.byte(TAG_BACKREF);
            payload.str(binding);
            payload.str(scope);
        }
        Relation::Recursive {
            anchor,
            step,
            accumulation,
        } => {
            payload.byte(TAG_BINDING);
            payload.byte(accumulation_byte(*accumulation));
            request_relation(payload, anchor);
            request_relation(payload, step);
        }
        Relation::Distinct(input) => {
            payload.byte(TAG_DISTINCT);
            request_relation(payload, input);
        }
    }
}

fn request_expr(payload: &mut RequestWriter, expression: &super::unbound::Expr) {
    use super::unbound::Expr;

    match expression {
        Expr::Literal(literal) => {
            payload.byte(TAG_EXPR_LITERAL);
            payload.optional_kind(literal.kind);
            request_raw_scalar(payload, &literal.raw);
        }
        Expr::Column { scope, name } => {
            payload.byte(TAG_EXPR_SLOT);
            payload.str(scope);
            payload.str(name);
        }
        Expr::Unary { op, expression } => {
            payload.byte(TAG_EXPR_UNARY);
            payload.byte(unary_byte(*op));
            request_expr(payload, expression);
        }
        Expr::Binary { op, left, right } => {
            payload.byte(TAG_EXPR_BINARY);
            payload.byte(binary_byte(*op));
            request_expr(payload, left);
            request_expr(payload, right);
        }
        Expr::Cast { expression, to } => {
            payload.byte(TAG_EXPR_CAST);
            payload.byte(kind_byte(*to));
            request_expr(payload, expression);
        }
        Expr::Branch { arms, otherwise } => {
            payload.byte(TAG_EXPR_BRANCH);
            payload.len(arms.len());
            for arm in arms {
                request_expr(payload, &arm.when);
                request_expr(payload, &arm.then);
            }
            request_expr(payload, otherwise);
        }
        Expr::TextMatch {
            value,
            parts,
            comparison,
        } => {
            payload.byte(TAG_EXPR_TEXT_MATCH);
            payload.byte(comparison_byte(*comparison));
            request_expr(payload, value);
            payload.len(parts.len());
            for part in parts {
                match part {
                    super::unbound::TextMatchPart::Literal(value) => {
                        payload.byte(1);
                        payload.str(value);
                    }
                    super::unbound::TextMatchPart::AnyMany => payload.byte(2),
                }
            }
        }
        Expr::Exists(relation) => {
            payload.byte(TAG_EXPR_EXISTS);
            request_relation(payload, relation);
        }
        Expr::First(relation) => {
            payload.byte(TAG_EXPR_FIRST);
            request_relation(payload, relation);
        }
        Expr::Scalar(relation) => {
            payload.byte(TAG_EXPR_SCALAR);
            request_relation(payload, relation);
        }
        Expr::Array(relation) => {
            payload.byte(TAG_EXPR_ARRAY);
            request_relation(payload, relation);
        }
    }
}

fn request_optional_str(payload: &mut RequestWriter, value: Option<&str>) {
    match value {
        Some(value) => {
            payload.byte(1);
            payload.str(value);
        }
        None => payload.byte(0),
    }
}

fn request_raw_scalar(payload: &mut RequestWriter, value: &super::unbound::RawScalar) {
    use super::unbound::RawScalar;

    match value {
        RawScalar::Null => payload.byte(VALUE_NULL),
        RawScalar::Text(value) => {
            payload.byte(VALUE_TEXT);
            payload.str(value);
        }
        RawScalar::Number(value) => {
            payload.byte(VALUE_INT64);
            payload.str(value);
        }
        RawScalar::Bool(value) => {
            payload.byte(VALUE_BOOL);
            payload.byte(u8::from(*value));
        }
    }
}

/// The family digest of one relation on its own, with no enclosing frames.
/// A relation stamped this way carries the identity it has wherever it
/// appears, so a count charged to it is comparable across statements.
pub fn relation_family(relation: &Relation) -> Fingerprint {
    relation_fingerprints(relation).family
}

pub fn relation_fingerprints(relation: &Relation) -> RelationFingerprints {
    Canonicalizer::new(&[]).relation(relation)
}

pub(crate) fn expressions<'a>(
    input: &super::RowType,
    expressions: impl IntoIterator<Item = &'a Expr>,
) -> Fingerprint {
    let expressions = expressions.into_iter().collect::<Vec<_>>();
    let mut payload = Pair::default();
    payload.u64(expressions.len() as u64);
    let mut canonicalizer = Canonicalizer::new(&[]);
    canonicalizer.scoped(input.slots(), |canonicalizer| {
        for expression in expressions {
            canonicalizer.expr(&mut payload, expression);
        }
    });
    finish(DOMAIN_EXPRESSIONS, &payload.exact)
}

pub(crate) fn finish(domain: u8, payload: &[u8]) -> Fingerprint {
    let mut hasher = Sha256::new();
    hasher.update([domain, CANONICALIZATION_VERSION]);
    hasher.update(payload);
    let full = hasher.finalize();
    let mut digest = [0u8; 16];
    digest.copy_from_slice(&full[..16]);
    Fingerprint {
        canonicalization_version: CANONICALIZATION_VERSION,
        hash_algorithm: HASH_SHA256_128,
        digest,
    }
}

/// Parallel exact/family canonical byte streams. The two encodings differ
/// only at literal positions.
#[derive(Default)]
struct Pair {
    exact: Vec<u8>,
    family: Vec<u8>,
}

impl Pair {
    fn byte(&mut self, value: u8) {
        self.exact.push(value);
        self.family.push(value);
    }

    fn u32(&mut self, value: u32) {
        self.exact.extend_from_slice(&value.to_be_bytes());
        self.family.extend_from_slice(&value.to_be_bytes());
    }

    fn u64(&mut self, value: u64) {
        self.exact.extend_from_slice(&value.to_be_bytes());
        self.family.extend_from_slice(&value.to_be_bytes());
    }

    fn str(&mut self, value: &str) {
        self.u64(value.len() as u64);
        self.exact.extend_from_slice(value.as_bytes());
        self.family.extend_from_slice(value.as_bytes());
    }

    fn digests(&mut self, digests: RelationFingerprints) {
        self.exact.extend_from_slice(&digests.exact.digest);
        self.family.extend_from_slice(&digests.family.digest);
    }

    fn literal(&mut self, value: &Value) {
        self.family.push(VALUE_PLACEHOLDER);
        self.family.push(scalar_type_byte(value.scalar_type()));
        match value {
            Value::Text(text) => {
                self.exact.push(VALUE_TEXT);
                self.exact
                    .extend_from_slice(&(text.len() as u64).to_be_bytes());
                self.exact.extend_from_slice(text.as_bytes());
            }
            Value::Int64(value) => {
                self.exact.push(VALUE_INT64);
                self.exact.extend_from_slice(&value.to_be_bytes());
            }
            Value::Float64(value) => {
                self.exact.push(VALUE_FLOAT64);
                self.exact.extend_from_slice(&value.to_bits().to_be_bytes());
            }
            Value::Bool(value) => {
                self.exact.push(VALUE_BOOL);
                self.exact.push(u8::from(*value));
            }
            Value::Null(scalar_type) => {
                self.exact.push(VALUE_NULL);
                self.exact.push(scalar_type_byte(*scalar_type));
            }
        }
    }

    /// A literal text fragment (text-match segments): exact bytes on the
    /// exact stream, a typed placeholder on the family stream.
    fn literal_text_fragment(&mut self, value: &str) {
        self.family.push(VALUE_PLACEHOLDER);
        self.family.push(scalar_type_byte(
            crate::engine::catalog::model::ScalarType::Text,
        ));
        self.exact.push(VALUE_TEXT);
        self.exact
            .extend_from_slice(&(value.len() as u64).to_be_bytes());
        self.exact.extend_from_slice(value.as_bytes());
    }
}

fn scalar_type_byte(value: crate::engine::catalog::model::ScalarType) -> u8 {
    use crate::engine::catalog::model::ScalarType;
    match value {
        ScalarType::Text => 1,
        ScalarType::Int64 => 2,
        ScalarType::Float64 => 3,
        ScalarType::Bool => 4,
    }
}

fn kind_byte(value: Kind) -> u8 {
    match value {
        Kind::Text => 1,
        Kind::Int64 => 2,
        Kind::Float64 => 3,
        Kind::Bool => 4,
        Kind::Row => 5,
        Kind::Array => 6,
    }
}

pub(crate) fn cardinality_byte(value: RootCardinality) -> u8 {
    match value {
        RootCardinality::Many => 1,
        RootCardinality::First => 2,
        RootCardinality::ExactlyOne => 3,
        RootCardinality::Scalar => 4,
    }
}

fn unary_byte(value: UnaryOp) -> u8 {
    match value {
        UnaryOp::Not => 1,
        UnaryOp::Negate => 2,
        UnaryOp::IsNull => 3,
        UnaryOp::IsNotNull => 4,
    }
}

fn binary_byte(value: BinaryOp) -> u8 {
    match value {
        BinaryOp::Eq => 1,
        BinaryOp::Ne => 2,
        BinaryOp::Lt => 3,
        BinaryOp::Lte => 4,
        BinaryOp::Gt => 5,
        BinaryOp::Gte => 6,
        BinaryOp::And => 7,
        BinaryOp::Or => 8,
        BinaryOp::Add => 9,
        BinaryOp::Sub => 10,
        BinaryOp::Mul => 11,
        BinaryOp::Div => 12,
    }
}

pub(crate) fn join_byte(value: JoinKind) -> u8 {
    match value {
        JoinKind::Inner => 1,
        JoinKind::Left => 2,
    }
}

pub(crate) fn quantifier_byte(value: SetQuantifier) -> u8 {
    match value {
        SetQuantifier::All => 1,
        SetQuantifier::Distinct => 2,
    }
}

pub(crate) fn accumulation_byte(value: RecursiveAccumulation) -> u8 {
    match value {
        RecursiveAccumulation::All => 1,
        RecursiveAccumulation::New => 2,
    }
}

pub(crate) fn aggregate_byte(value: AggregateFunction) -> u8 {
    match value {
        AggregateFunction::Count => 1,
        AggregateFunction::Sum => 2,
        AggregateFunction::Average => 3,
        AggregateFunction::Min => 4,
        AggregateFunction::Max => 5,
    }
}

fn comparison_byte(value: TextComparison) -> u8 {
    match value {
        TextComparison::Exact => 1,
        TextComparison::UnicodeSimpleFold => 2,
    }
}

struct Canonicalizer<'a> {
    bindings: &'a [bound::Binding],
    positions: HashMap<&'a str, usize>,
    /// Binding position → canonical index, assigned at first reference.
    canonical: HashMap<usize, u32>,
    /// Binding position → digests, memoized after the definition encodes.
    finished: HashMap<usize, RelationFingerprints>,
    /// Binding positions in canonical (first-reference) order.
    order: Vec<usize>,
    /// Slot frames from outermost to innermost. A slot encodes as its
    /// distance out through this stack and its position within that frame,
    /// so absolute slot numbers never reach the digest and a subtree hashes
    /// identically wherever it sits.
    frames: Vec<Vec<SlotId>>,
    subtrees: Vec<RelationFingerprints>,
    tables: BTreeSet<SchemaId>,
    binding_roots: Vec<(String, RelationFingerprints)>,
}

impl<'a> Canonicalizer<'a> {
    fn new(bindings: &'a [bound::Binding]) -> Self {
        let positions = bindings
            .iter()
            .enumerate()
            .map(|(position, binding)| (binding.name.as_str(), position))
            .collect();
        Self {
            bindings,
            positions,
            canonical: HashMap::new(),
            finished: HashMap::new(),
            order: Vec::new(),
            frames: Vec::new(),
            subtrees: Vec::new(),
            tables: BTreeSet::new(),
            binding_roots: Vec::new(),
        }
    }

    fn slot_reference(&self, payload: &mut Pair, slot: SlotId) {
        for (levels_out, frame) in self.frames.iter().rev().enumerate() {
            if let Some(index) = frame.iter().position(|candidate| *candidate == slot) {
                payload.u32(levels_out as u32);
                payload.u32(index as u32);
                return;
            }
        }
        payload.u32(u32::MAX);
        payload.u32(u32::MAX);
    }

    fn scoped(&mut self, frame: Vec<SlotId>, encode: impl FnOnce(&mut Self)) {
        self.frames.push(frame);
        encode(self);
        self.frames.pop();
    }

    fn relation(&mut self, relation: &Relation) -> RelationFingerprints {
        let mut payload = Pair::default();
        match &relation.node {
            RelationNode::Scan { table, .. } => {
                payload.byte(TAG_SCAN);
                payload.u32(table.schema_id.get());
                payload.u64(relation.output().fields.len() as u64);
                self.tables.insert(table.schema_id);
            }
            RelationNode::Rows { values, .. } => {
                payload.byte(TAG_ROWS);
                let output = relation.output().clone();
                payload.u64(output.fields.len() as u64);
                for field in &output.fields {
                    payload.str(&field.name);
                    payload.byte(kind_byte(field.value_type.kind));
                    payload.byte(u8::from(field.value_type.nullable));
                }
                payload.u64(values.len() as u64);
                for row in values {
                    for value in row {
                        payload.literal(value);
                    }
                }
            }
            RelationNode::Filter { input, predicate } => {
                let digests = self.relation(input);
                payload.byte(TAG_FILTER);
                payload.digests(digests);
                self.scoped(input.output().slots(), |canonicalizer| {
                    canonicalizer.expr(&mut payload, predicate);
                });
            }
            RelationNode::Project { input, fields, .. } => {
                let digests = self.relation(input);
                payload.byte(TAG_PROJECT);
                payload.digests(digests);
                payload.u64(fields.len() as u64);
                self.scoped(input.output().slots(), |canonicalizer| {
                    for field in fields {
                        payload.str(&field.name);
                        canonicalizer.expr(&mut payload, &field.expression);
                    }
                });
            }
            RelationNode::Join {
                left,
                right,
                kind,
                on,
            } => {
                let left_digests = self.relation(left);
                let right_digests = self.relation(right);
                payload.byte(TAG_JOIN);
                payload.byte(join_byte(*kind));
                payload.digests(left_digests);
                payload.digests(right_digests);
                let mut frame = left.output().slots();
                frame.extend(right.output().slots());
                self.scoped(frame, |canonicalizer| {
                    canonicalizer.expr(&mut payload, on);
                });
            }
            RelationNode::Concatenate { inputs, .. } => {
                let digests: Vec<_> = inputs.iter().map(|input| self.relation(input)).collect();
                payload.byte(TAG_CONCATENATE);
                payload.u64(digests.len() as u64);
                for input in digests {
                    payload.digests(input);
                }
            }
            RelationNode::Intersect {
                left,
                right,
                quantifier,
                ..
            } => {
                let left_digests = self.relation(left);
                let right_digests = self.relation(right);
                payload.byte(TAG_INTERSECT);
                payload.byte(quantifier_byte(*quantifier));
                payload.digests(left_digests);
                payload.digests(right_digests);
            }
            RelationNode::Except {
                left,
                right,
                quantifier,
                ..
            } => {
                let left_digests = self.relation(left);
                let right_digests = self.relation(right);
                payload.byte(TAG_EXCEPT);
                payload.byte(quantifier_byte(*quantifier));
                payload.digests(left_digests);
                payload.digests(right_digests);
            }
            RelationNode::Aggregate {
                input,
                groups,
                terms,
            } => {
                let digests = self.relation(input);
                payload.byte(TAG_AGGREGATE);
                payload.digests(digests);
                payload.u64(groups.len() as u64);
                payload.u64(terms.len() as u64);
                self.scoped(input.output().slots(), |canonicalizer| {
                    for group in groups {
                        payload.str(&group.name);
                        canonicalizer.expr(&mut payload, &group.expression);
                    }
                    for term in terms {
                        payload.byte(aggregate_byte(term.function));
                        match &term.argument {
                            Some(argument) => {
                                payload.byte(1);
                                canonicalizer.expr(&mut payload, argument);
                            }
                            None => payload.byte(0),
                        }
                        payload.str(&term.name);
                    }
                });
            }
            RelationNode::Order { input, terms } => {
                let digests = self.relation(input);
                payload.byte(TAG_ORDER);
                payload.digests(digests);
                payload.u64(terms.len() as u64);
                self.scoped(input.output().slots(), |canonicalizer| {
                    for term in terms {
                        canonicalizer.expr(&mut payload, &term.expression);
                        payload.byte(u8::from(term.descending));
                    }
                });
            }
            RelationNode::Slice {
                input,
                offset,
                limit,
            } => {
                let digests = self.relation(input);
                payload.byte(TAG_SLICE);
                payload.digests(digests);
                payload.u64(*offset as u64);
                match limit {
                    Some(limit) => {
                        payload.byte(1);
                        payload.u64(*limit as u64);
                    }
                    None => payload.byte(0),
                }
            }
            RelationNode::Ref { binding, .. } | RelationNode::RecursiveRef { binding, .. } => {
                self.reference(&mut payload, binding);
                payload.u64(relation.output().fields.len() as u64);
            }
            RelationNode::Distinct(input) => {
                let digests = self.relation(input);
                payload.byte(TAG_DISTINCT);
                payload.digests(digests);
            }
        }
        let digests = RelationFingerprints {
            exact: finish(DOMAIN_EXACT, &payload.exact),
            family: finish(DOMAIN_FAMILY, &payload.family),
        };
        self.subtrees.push(digests);
        digests
    }

    /// Encode a binding reference. A reference to a finished binding carries
    /// the binding's digests; a reference to a binding whose definition is
    /// still on the encoding stack (recursion) carries its canonical index
    /// instead, which is assigned before the body encodes and is therefore
    /// always available.
    /// A name outside the query's bindings references an earlier statement's
    /// result: it encodes as an external reference whose identity is only the
    /// referenced shape, because statement names are not semantic and
    /// cross-statement identity belongs to the program fingerprint.
    fn reference(&mut self, payload: &mut Pair, name: &str) {
        let Some(&position) = self.positions.get(name) else {
            payload.byte(TAG_EXTERNAL_REF);
            return;
        };
        if let Some(&digests) = self.finished.get(&position) {
            payload.byte(TAG_REF);
            payload.digests(digests);
            return;
        }
        if let Some(&canonical) = self.canonical.get(&position) {
            payload.byte(TAG_BACKREF);
            payload.u32(canonical);
            return;
        }
        let digests = self.binding(position);
        payload.byte(TAG_REF);
        payload.digests(digests);
    }

    fn binding(&mut self, position: usize) -> RelationFingerprints {
        let canonical = self.order.len() as u32;
        self.canonical.insert(position, canonical);
        self.order.push(position);

        let binding = &self.bindings[position];
        let name = binding.name.clone();
        let root = self.relation(&binding.root);
        self.binding_roots.push((name, root));
        let mut payload = Pair::default();
        payload.byte(TAG_BINDING);
        payload.digests(root);
        match (&binding.step, binding.accumulation) {
            (Some(step), Some(accumulation)) => {
                payload.byte(1);
                payload.byte(accumulation_byte(accumulation));
                let step = self.relation(step);
                payload.digests(step);
            }
            _ => payload.byte(0),
        }

        let digests = RelationFingerprints {
            exact: finish(DOMAIN_EXACT, &payload.exact),
            family: finish(DOMAIN_FAMILY, &payload.family),
        };
        self.finished.insert(position, digests);
        digests
    }

    fn expr(&mut self, payload: &mut Pair, expr: &Expr) {
        match expr {
            Expr::Literal(value) => {
                payload.byte(TAG_EXPR_LITERAL);
                payload.literal(value);
            }
            Expr::SlotRef { slot, .. } => {
                payload.byte(TAG_EXPR_SLOT);
                self.slot_reference(payload, *slot);
            }
            Expr::Unary { op, expression, .. } => {
                payload.byte(TAG_EXPR_UNARY);
                payload.byte(unary_byte(*op));
                self.expr(payload, expression);
            }
            Expr::Binary {
                op, left, right, ..
            } => {
                payload.byte(TAG_EXPR_BINARY);
                payload.byte(binary_byte(*op));
                self.expr(payload, left);
                self.expr(payload, right);
            }
            Expr::Cast { expression, to, .. } => {
                payload.byte(TAG_EXPR_CAST);
                payload.byte(kind_byte(*to));
                self.expr(payload, expression);
            }
            Expr::Branch {
                arms, otherwise, ..
            } => {
                payload.byte(TAG_EXPR_BRANCH);
                payload.u64(arms.len() as u64);
                for arm in arms {
                    self.expr(payload, &arm.when);
                    self.expr(payload, &arm.then);
                }
                self.expr(payload, otherwise);
            }
            Expr::TextMatch { value, pattern, .. } => {
                payload.byte(TAG_EXPR_TEXT_MATCH);
                payload.byte(comparison_byte(pattern.comparison()));
                payload.byte(u8::from(pattern.leading()));
                payload.byte(u8::from(pattern.trailing()));
                payload.u64(pattern.segments().len() as u64);
                for segment in pattern.segments() {
                    payload.literal_text_fragment(segment);
                }
                self.expr(payload, value);
            }
            Expr::Exists(relation) => {
                let digests = self.relation(relation);
                payload.byte(TAG_EXPR_EXISTS);
                payload.digests(digests);
            }
            Expr::First { relation, .. } => {
                let digests = self.relation(relation);
                payload.byte(TAG_EXPR_FIRST);
                payload.digests(digests);
            }
            Expr::Scalar { relation, .. } => {
                let digests = self.relation(relation);
                payload.byte(TAG_EXPR_SCALAR);
                payload.digests(digests);
            }
            Expr::Array { relation, .. } => {
                let digests = self.relation(relation);
                payload.byte(TAG_EXPR_ARRAY);
                payload.digests(digests);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::engine::catalog::identity::{
        DefinitionGeneration, ExistenceGeneration, SchemaId, StorageGeneration, ValueGeneration,
        WriteProtocolGeneration,
    };
    use crate::engine::catalog::model::{Column, ScalarType, Table};

    use super::super::bound::Relation;
    use super::super::{Field, RootCardinality, RowType, Type};
    use super::*;

    fn table(name: &str, schema_id: u32) -> Table {
        Table {
            id: format!("t{schema_id}").into(),
            schema_id: SchemaId::new(schema_id).unwrap(),
            name: name.into(),
            definition_generation: DefinitionGeneration::ZERO,
            existence_generation: ExistenceGeneration::ZERO,
            write_protocol_generation: WriteProtocolGeneration::ZERO,
            storage_generation: StorageGeneration::INITIAL,
            columns: [
                ("id", ScalarType::Text, false),
                ("status", ScalarType::Text, false),
                ("priority", ScalarType::Int64, false),
            ]
            .into_iter()
            .enumerate()
            .map(|(index, (name, scalar_type, nullable))| Column {
                id: format!("c{}", index + 1).into(),
                schema_id: SchemaId::new((index + 1) as u32).unwrap(),
                name: name.into(),
                value_generation: ValueGeneration::ZERO,
                scalar_type,
                nullable,
                format: String::new(),
                insert_default: None,
                missing_value: None,
            })
            .collect(),
            primary_key: vec!["id".into()],
            indexes: Vec::new(),
            foreign_keys: Vec::new(),
            constraints: Vec::new(),
        }
    }

    fn scan(scope: &str, name: &str, schema_id: u32, first_slot: usize) -> Relation {
        Relation::scan(
            table(name, schema_id),
            scope,
            (first_slot..first_slot + 3).map(SlotId).collect(),
        )
    }

    fn status_filter(scope: &str, first_slot: usize, status: &str) -> Relation {
        let input = scan(scope, "tasks", 7, first_slot);
        let slot = input.output().lookup("status").unwrap().slot;
        Relation::filter(
            input,
            Expr::binary(
                BinaryOp::Eq,
                Expr::slot(slot, "status", Type::scalar(Kind::Text, false)),
                Expr::literal(Value::Text(status.into())),
            ),
        )
    }

    fn fingerprint_query(root: Relation) -> QueryFingerprints {
        query(&bound::Query {
            root,
            cardinality: RootCardinality::Many,
            bindings: Vec::new(),
            next_slot: SlotId(100),
        })
    }

    #[test]
    fn scope_labels_and_slot_numbering_do_not_affect_identity() {
        let left = fingerprint_query(status_filter("t", 0, "open"));
        let right = fingerprint_query(status_filter("completely_different", 40, "open"));
        assert_eq!(left.exact, right.exact);
        assert_eq!(left.family, right.family);
        assert_eq!(left.subtrees, right.subtrees);
    }

    #[test]
    fn table_rename_preserves_identity_and_schema_id_defines_it() {
        let named_tasks = fingerprint_query(status_filter("t", 0, "open"));
        let renamed = {
            let input = scan("t", "chores", 7, 0);
            let slot = input.output().lookup("status").unwrap().slot;
            fingerprint_query(Relation::filter(
                input,
                Expr::binary(
                    BinaryOp::Eq,
                    Expr::slot(slot, "status", Type::scalar(Kind::Text, false)),
                    Expr::literal(Value::Text("open".into())),
                ),
            ))
        };
        assert_eq!(named_tasks.exact, renamed.exact);

        let other_table = {
            let input = scan("t", "tasks", 9, 0);
            let slot = input.output().lookup("status").unwrap().slot;
            fingerprint_query(Relation::filter(
                input,
                Expr::binary(
                    BinaryOp::Eq,
                    Expr::slot(slot, "status", Type::scalar(Kind::Text, false)),
                    Expr::literal(Value::Text("open".into())),
                ),
            ))
        };
        assert_ne!(named_tasks.exact, other_table.exact);
        assert_ne!(named_tasks.family, other_table.family);
    }

    #[test]
    fn literals_split_exact_from_family() {
        let open = fingerprint_query(status_filter("t", 0, "open"));
        let done = fingerprint_query(status_filter("t", 0, "done"));
        assert_ne!(open.exact, done.exact);
        assert_eq!(open.family, done.family);
    }

    #[test]
    fn structure_changes_both_fingerprints() {
        let filtered = fingerprint_query(status_filter("t", 0, "open"));
        let unfiltered = fingerprint_query(scan("t", "tasks", 7, 0));
        assert_ne!(filtered.exact, unfiltered.exact);
        assert_ne!(filtered.family, unfiltered.family);
    }

    #[test]
    fn dependency_set_holds_scanned_schema_ids() {
        let digests = fingerprint_query(status_filter("t", 0, "open"));
        let tables: Vec<u32> = digests.tables.iter().map(|id| id.get()).collect();
        assert_eq!(tables, vec![7]);
    }

    #[test]
    fn binding_names_do_not_affect_identity() {
        let build = |name: &str| {
            let binding_root = status_filter("inner", 0, "open");
            let output = binding_root.output().clone();
            let occurrence: Vec<Field> = output
                .fields
                .iter()
                .enumerate()
                .map(|(index, field)| Field {
                    name: field.name.clone(),
                    slot: SlotId(50 + index),
                    value_type: field.value_type.clone(),
                })
                .collect();
            let reference = Relation::reference(name, "outer", occurrence, output.slots());
            let binding = bound::Binding {
                name: name.into(),
                root: binding_root,
                output,
                plan_sensitive: false,
                recursive: false,
                step: None,
                accumulation: None,
            };
            query(&bound::Query {
                root: reference,
                cardinality: RootCardinality::Many,
                bindings: vec![binding],
                next_slot: SlotId(100),
            })
        };
        let left = build("alpha");
        let right = build("omega");
        assert_eq!(left.exact, right.exact);
        assert_eq!(left.family, right.family);
    }

    #[test]
    fn subtree_fingerprints_are_shared_across_different_queries() {
        let plain = fingerprint_query(status_filter("t", 0, "open"));
        let sliced = fingerprint_query(Relation::slice(status_filter("x", 5, "open"), 0, Some(10)));
        let plain_root = plain.subtrees.last().unwrap();
        assert!(sliced.subtrees.contains(plain_root));
    }

    fn join_of(left: Relation, right: Relation) -> Relation {
        let left_id = left.output().lookup("id").unwrap().slot;
        let right_id = right.output().lookup("id").unwrap().slot;
        Relation::join(
            left,
            right,
            JoinKind::Inner,
            Expr::binary(
                BinaryOp::Eq,
                Expr::slot(left_id, "id", Type::scalar(Kind::Text, false)),
                Expr::slot(right_id, "id", Type::scalar(Kind::Text, false)),
            ),
        )
    }

    #[test]
    fn a_subtree_fingerprints_the_same_wherever_it_sits() {
        let standalone = fingerprint_query(status_filter("t", 0, "open"));
        let subject = *standalone.subtrees.last().unwrap();

        let as_left = fingerprint_query(join_of(
            status_filter("l", 0, "open"),
            scan("r", "boards", 8, 30),
        ));
        let as_right = fingerprint_query(join_of(
            scan("l", "boards", 8, 30),
            status_filter("r", 60, "open"),
        ));
        let nested_deep = fingerprint_query(Relation::slice(
            Relation::distinct(status_filter("x", 90, "open")),
            5,
            Some(2),
        ));

        assert!(
            as_left.subtrees.contains(&subject),
            "the same filtered scan must be recognised on the left of a join"
        );
        assert!(
            as_right.subtrees.contains(&subject),
            "and on the right, where the enclosing frame differs"
        );
        assert!(
            nested_deep.subtrees.contains(&subject),
            "and under unrelated parents"
        );
    }

    #[test]
    fn dropping_absolute_slots_does_not_collapse_distinct_shapes() {
        let by_column = |column: &str| {
            let input = scan("t", "tasks", 7, 0);
            let slot = input.output().lookup(column).unwrap().slot;
            fingerprint_query(Relation::filter(
                input,
                Expr::binary(
                    BinaryOp::Eq,
                    Expr::slot(slot, column, Type::scalar(Kind::Text, false)),
                    Expr::literal(Value::Text("x".into())),
                ),
            ))
        };
        assert_ne!(
            by_column("id").exact,
            by_column("status").exact,
            "a predicate on a different column is a different computation"
        );

        let tasks = || scan("a", "tasks", 7, 0);
        let boards = || scan("b", "boards", 8, 20);
        assert_ne!(
            fingerprint_query(join_of(tasks(), boards())).exact,
            fingerprint_query(join_of(boards(), tasks())).exact,
            "join operand order is not canonicalised away"
        );

        let self_join =
            fingerprint_query(join_of(scan("a", "tasks", 7, 0), scan("b", "tasks", 7, 20)));
        assert_ne!(
            self_join.exact,
            fingerprint_query(join_of(tasks(), boards())).exact
        );
    }

    #[test]
    fn correlation_depth_is_part_of_identity() {
        let correlated = |outer_column: &str| {
            let outer = scan("o", "tasks", 7, 0);
            let outer_slot = outer.output().lookup(outer_column).unwrap().slot;
            let inner = scan("i", "boards", 8, 40);
            let inner_slot = inner.output().lookup("id").unwrap().slot;
            let inner_filtered = Relation::filter(
                inner,
                Expr::binary(
                    BinaryOp::Eq,
                    Expr::slot(inner_slot, "id", Type::scalar(Kind::Text, false)),
                    Expr::slot(outer_slot, outer_column, Type::scalar(Kind::Text, false)),
                ),
            );
            fingerprint_query(Relation::filter(outer, Expr::exists(inner_filtered)))
        };

        let uncorrelated = {
            let outer = scan("o", "tasks", 7, 0);
            let inner = scan("i", "boards", 8, 40);
            let inner_slot = inner.output().lookup("id").unwrap().slot;
            let inner_filtered = Relation::filter(
                inner,
                Expr::binary(
                    BinaryOp::Eq,
                    Expr::slot(inner_slot, "id", Type::scalar(Kind::Text, false)),
                    Expr::literal(Value::Text("x".into())),
                ),
            );
            fingerprint_query(Relation::filter(outer, Expr::exists(inner_filtered)))
        };

        assert_eq!(
            correlated("id").exact,
            correlated("id").exact,
            "the same correlation is stable"
        );
        assert_ne!(
            correlated("id").exact,
            correlated("status").exact,
            "correlating on a different outer column is a different computation"
        );
        assert_ne!(
            correlated("id").exact,
            uncorrelated.exact,
            "a correlated crossing differs from an uncorrelated one"
        );
    }

    #[test]
    fn absolute_slot_numbering_never_reaches_the_digest() {
        let build = |first_slot: usize| {
            let input = scan("t", "tasks", 7, first_slot);
            let status = input.output().lookup("status").unwrap().slot;
            let priority = input.output().lookup("priority").unwrap().slot;
            let ordered = Relation::order(
                Relation::filter(
                    input,
                    Expr::binary(
                        BinaryOp::Eq,
                        Expr::slot(status, "status", Type::scalar(Kind::Text, false)),
                        Expr::literal(Value::Text("open".into())),
                    ),
                ),
                vec![bound::BoundOrderTerm {
                    expression: Expr::slot(priority, "priority", Type::scalar(Kind::Int64, false)),
                    descending: true,
                }],
            );
            fingerprint_query(ordered)
        };
        assert_eq!(build(0).exact, build(1_000).exact);
        assert_eq!(build(0).subtrees, build(1_000).subtrees);
    }

    fn request_query(status: &str) -> super::super::unbound::Query {
        use super::super::unbound;

        unbound::Query {
            root: unbound::Relation::Filter {
                input: Box::new(unbound::Relation::Scan {
                    table: "tasks".into(),
                    scope: "task".into(),
                }),
                predicate: unbound::Expr::Binary {
                    op: BinaryOp::Eq,
                    left: Box::new(unbound::Expr::Column {
                        scope: "task".into(),
                        name: "status".into(),
                    }),
                    right: Box::new(unbound::Expr::Literal(unbound::Literal {
                        raw: unbound::RawScalar::Text(status.into()),
                        kind: Some(Kind::Text),
                    })),
                },
            },
            cardinality: RootCardinality::Many,
            bindings: HashMap::new(),
        }
    }

    #[test]
    fn request_identity_is_stable_and_literal_sensitive() {
        assert_eq!(
            request(&request_query("open")),
            request(&request_query("open"))
        );
        assert_ne!(
            request(&request_query("open")),
            request(&request_query("done"))
        );
    }

    #[test]
    fn request_identity_ignores_binding_map_order() {
        let mut left = request_query("open");
        left.bindings.insert(
            "first".into(),
            super::super::unbound::Relation::Scan {
                table: "users".into(),
                scope: "user".into(),
            },
        );
        left.bindings.insert(
            "second".into(),
            super::super::unbound::Relation::Scan {
                table: "boards".into(),
                scope: "board".into(),
            },
        );
        let mut right = request_query("open");
        for name in ["second", "first"] {
            right
                .bindings
                .insert(name.into(), left.bindings[name].clone());
        }
        assert_eq!(request(&left), request(&right));
    }

    #[test]
    fn expression_identity_normalizes_slot_numbers() {
        let left_input = RowType {
            fields: vec![
                Field {
                    name: "first".into(),
                    slot: SlotId(4),
                    value_type: Type::scalar(Kind::Text, false),
                },
                Field {
                    name: "second".into(),
                    slot: SlotId(8),
                    value_type: Type::scalar(Kind::Int64, false),
                },
            ],
        };
        let right_input = RowType {
            fields: vec![
                Field {
                    name: "first".into(),
                    slot: SlotId(40),
                    value_type: Type::scalar(Kind::Text, false),
                },
                Field {
                    name: "second".into(),
                    slot: SlotId(80),
                    value_type: Type::scalar(Kind::Int64, false),
                },
            ],
        };
        let left = Expr::slot(SlotId(8), "second", Type::scalar(Kind::Int64, false));
        let right = Expr::slot(SlotId(80), "second", Type::scalar(Kind::Int64, false));
        let other = Expr::slot(SlotId(40), "first", Type::scalar(Kind::Text, false));

        assert_eq!(
            expressions(&left_input, [&left]),
            expressions(&right_input, [&right])
        );
        assert_ne!(
            expressions(&right_input, [&right]),
            expressions(&right_input, [&other])
        );
    }

    #[test]
    fn golden_vector_pins_the_canonical_encoding() {
        let digests = fingerprint_query(status_filter("t", 0, "open"));
        assert_eq!(
            digests.exact.to_string(),
            "c1h1:8da8e009807e703969973c696286c374",
        );
        assert_eq!(
            digests.family.to_string(),
            "c1h1:465bf41eb5caa1f4d523be67ebba413d",
        );
    }
}
