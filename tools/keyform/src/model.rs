//! Parsed representation of a keyform specification.
//!
//! The model is deliberately declaration-shaped rather than expression-shaped:
//! structural grammars carry authored guard and layout prose alongside the
//! machine-checkable parts (tags, vectors, laws, host bindings), and the
//! emitter consumes only the machine-checkable parts.

#[derive(Debug, PartialEq)]
pub struct Spec {
    pub name: String,
    pub version: u32,
    pub domains: Vec<Domain>,
    pub codecs: Vec<Codec>,
    pub records: Vec<Record>,
    pub fixtures: Vec<Fixture>,
    pub keyspaces: Vec<Keyspace>,
}

#[derive(Debug, PartialEq)]
pub struct Domain {
    pub name: String,
    pub comparator: Option<String>,
    pub cases: Vec<String>,
}

#[derive(Debug, PartialEq)]
pub struct Codec {
    pub name: String,
    pub value: Option<String>,
    pub refines: Option<String>,
    pub kernels: Vec<Kernel>,
    pub laws: Vec<Law>,
    pub notes: Vec<String>,
    pub rules: Vec<String>,
    pub cases: Vec<Case>,
    pub vectors: Vec<Vector>,
}

/// A host binding: `role` names how the emitter uses the path (`encode`,
/// `decode`, `set`, `read`, `remove`, `module`), and `convention` names the
/// calling shape the emitter must glue to.
#[derive(Debug, PartialEq)]
pub struct Kernel {
    pub role: String,
    pub path: String,
    pub convention: Option<String>,
}

#[derive(Debug, PartialEq)]
pub struct Law {
    pub name: String,
    pub comparator: Option<String>,
}

/// One alternative of a tagged scalar encoding, with its host bindings.
#[derive(Debug, PartialEq)]
pub struct Case {
    pub name: String,
    pub tag: Option<u8>,
    pub const_name: Option<String>,
    pub encoder: Option<Encoder>,
    pub payload: Option<String>,
    pub escape: Option<(Vec<u8>, Vec<u8>)>,
    pub terminator: Option<Vec<u8>>,
}

#[derive(Debug, PartialEq)]
pub struct Encoder {
    pub name: String,
    pub form: EncoderForm,
}

/// Return shape of a per-case kernel encoder.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum EncoderForm {
    Array,
    OptionArray,
    Vec,
}

#[derive(Debug, PartialEq)]
pub enum Vector {
    Accept {
        bytes: Vec<u8>,
        value: SpecValue,
        note: Option<String>,
    },
    Reject {
        bytes: Vec<u8>,
        note: String,
    },
    /// Ascending chain of values whose encodings must sort in the same order.
    Order {
        values: Vec<SpecValue>,
    },
    /// Prefix scan bound: `prefix` maps to `end`, or to no bound at all.
    Bound {
        prefix: Vec<u8>,
        end: Option<Vec<u8>>,
    },
}

#[derive(Debug, PartialEq)]
pub enum SpecValue {
    Null,
    Bool(bool),
    /// Wide enough for both int64 vector values and u64 varint values.
    Int(i128),
    /// Raw source spelling so `-0.0` and exponent forms emit verbatim.
    Float(String),
    Text(String),
    Bytes(Vec<u8>),
    Row(Vec<(String, SpecValue)>),
}

#[derive(Debug, PartialEq)]
pub struct Record {
    pub name: String,
    pub items: Vec<RecordItem>,
    pub kernels: Vec<Kernel>,
    pub laws: Vec<Law>,
    pub fixture: Option<String>,
    pub notes: Vec<String>,
    pub rules: Vec<String>,
    pub vectors: Vec<Vector>,
}

#[derive(Debug, PartialEq)]
pub enum RecordItem {
    Magic(Vec<u8>),
    Field {
        name: String,
        codec: String,
        doc: Option<String>,
    },
    Let {
        name: String,
        expr: String,
    },
    Guard(String),
    Bound(String),
    When {
        condition: String,
        items: Vec<RecordItem>,
    },
    Repeat {
        name: String,
        count: String,
        items: Vec<RecordItem>,
    },
    Eof,
}

/// The concrete table the conformance suite instantiates for record laws.
#[derive(Debug, PartialEq)]
pub struct Fixture {
    pub name: String,
    pub columns: Vec<FixtureColumn>,
}

#[derive(Debug, PartialEq)]
pub struct FixtureColumn {
    pub id: String,
    pub schema_id: u32,
    pub column_name: String,
    pub scalar: String,
}

#[derive(Debug, PartialEq)]
pub struct Keyspace {
    pub name: String,
    /// Root magic prefixing every durable key.
    pub magic: Vec<u8>,
    pub spaces: Vec<Space>,
    /// Permanently reserved tags that must never be allocated.
    pub reserved: Vec<(u8, String)>,
    pub scan: Option<Scan>,
}

#[derive(Debug, PartialEq)]
pub struct Space {
    pub name: String,
    /// Permanent binary keyspace tag following the root magic.
    pub tag: u8,
    pub fields: Vec<KeyField>,
    pub value: ValueForm,
    pub invariants: Vec<String>,
    pub notes: Vec<String>,
    pub vectors: Vec<KeyVector>,
}

#[derive(Debug, PartialEq)]
pub struct KeyField {
    pub name: String,
    pub kind: SegmentKind,
}

/// Binary encoding of one key segment.
#[derive(Clone, Debug, PartialEq)]
pub enum SegmentKind {
    /// Kind-prefixed physical identity ("t42"): the numeric part as uvarint.
    PhysicalId(String),
    Uvarint,
    /// Fixed-width big-endian, for segments whose scan order must equal
    /// numeric order.
    Be32,
    Be64,
    /// Encoded semantic-scalar tuple bytes; every scalar self-delimits, so
    /// tuples may only appear in the terminal run of a key.
    Tuple,
    /// Uvarint-length-framed bytes, usable before further segments.
    LenBytes,
    /// Terminal raw UTF-8 text.
    Text,
    /// Terminal raw bytes.
    Bytes,
}

/// Whole-key golden vector: structured field values and their exact bytes.
#[derive(Debug, PartialEq)]
pub struct KeyVector {
    pub fields: Vec<(String, KeyFieldValue)>,
    pub bytes: Vec<u8>,
    pub note: Option<String>,
}

#[derive(Debug, PartialEq)]
pub enum KeyFieldValue {
    Number(u64),
    Bytes(Vec<u8>),
    Text(String),
}

#[derive(Debug, PartialEq)]
pub enum ValueForm {
    Record(String),
    /// Reference to a normative storage JSON Schema, e.g. "catalog/table.v1".
    JsonSchema(String),
    Codec(String),
    Opaque(String),
}

#[derive(Debug, PartialEq)]
pub struct Scan {
    pub kernel: String,
    pub law: String,
    pub bounds: Vec<Vector>,
}

impl Spec {
    pub fn codec(&self, name: &str) -> Option<&Codec> {
        self.codecs.iter().find(|codec| codec.name == name)
    }

    pub fn fixture(&self, name: &str) -> Option<&Fixture> {
        self.fixtures.iter().find(|fixture| fixture.name == name)
    }
}

impl Codec {
    pub fn kernel(&self, role: &str) -> Option<&Kernel> {
        self.kernels.iter().find(|kernel| kernel.role == role)
    }

    pub fn law(&self, name: &str) -> Option<&Law> {
        self.laws.iter().find(|law| law.name == name)
    }
}

impl Record {
    pub fn kernel(&self, role: &str) -> Option<&Kernel> {
        self.kernels.iter().find(|kernel| kernel.role == role)
    }

    pub fn law(&self, name: &str) -> Option<&Law> {
        self.laws.iter().find(|law| law.name == name)
    }
}
