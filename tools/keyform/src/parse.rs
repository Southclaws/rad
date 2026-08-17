//! Lexer and recursive-descent parser for keyform specifications.

use std::fmt;

use crate::model::*;

#[derive(Debug)]
pub struct ParseError {
    pub line: usize,
    pub message: String,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "line {}: {}", self.line, self.message)
    }
}

impl std::error::Error for ParseError {}

type Result<T> = std::result::Result<T, ParseError>;

#[derive(Clone, Debug, PartialEq)]
enum Token {
    Ident(String),
    Str(String),
    Number(String),
    HexBytes(Vec<u8>),
    LBrace,
    RBrace,
    Eq,
    Arrow,
    Lt,
    Comma,
}

#[derive(Clone, Debug)]
struct Spanned {
    token: Token,
    line: usize,
}

fn lex(input: &str) -> Result<Vec<Spanned>> {
    let mut tokens = Vec::new();
    let mut line = 1;
    let bytes = input.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        match c {
            b'\n' => {
                line += 1;
                i += 1;
            }
            b' ' | b'\t' | b'\r' => i += 1,
            b'#' => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'{' => {
                tokens.push(Spanned {
                    token: Token::LBrace,
                    line,
                });
                i += 1;
            }
            b'}' => {
                tokens.push(Spanned {
                    token: Token::RBrace,
                    line,
                });
                i += 1;
            }
            b'=' => {
                tokens.push(Spanned {
                    token: Token::Eq,
                    line,
                });
                i += 1;
            }
            b'<' => {
                tokens.push(Spanned {
                    token: Token::Lt,
                    line,
                });
                i += 1;
            }
            b',' => {
                tokens.push(Spanned {
                    token: Token::Comma,
                    line,
                });
                i += 1;
            }
            b'-' if bytes.get(i + 1) == Some(&b'>') => {
                tokens.push(Spanned {
                    token: Token::Arrow,
                    line,
                });
                i += 2;
            }
            b'-' | b'0'..=b'9' => {
                let start = i;
                i += 1;
                while i < bytes.len()
                    && (bytes[i].is_ascii_digit()
                        || bytes[i] == b'.'
                        || bytes[i] == b'e'
                        || bytes[i] == b'E'
                        || ((bytes[i] == b'+' || bytes[i] == b'-')
                            && matches!(bytes[i - 1], b'e' | b'E')))
                {
                    i += 1;
                }
                let text = &input[start..i];
                tokens.push(Spanned {
                    token: Token::Number(text.to_owned()),
                    line,
                });
            }
            b'x' if bytes.get(i + 1) == Some(&b'"') => {
                i += 2;
                let mut hex = String::new();
                loop {
                    let Some(&c) = bytes.get(i) else {
                        return Err(error(line, "unterminated hex byte literal"));
                    };
                    i += 1;
                    match c {
                        b'"' => break,
                        b' ' => {}
                        b'\n' => return Err(error(line, "newline in hex byte literal")),
                        _ => hex.push(c as char),
                    }
                }
                if !hex.len().is_multiple_of(2) {
                    return Err(error(line, "hex byte literal needs an even digit count"));
                }
                let mut decoded = Vec::with_capacity(hex.len() / 2);
                for pair in hex.as_bytes().chunks_exact(2) {
                    let text = std::str::from_utf8(pair).expect("hex digits are ASCII");
                    let byte = u8::from_str_radix(text, 16)
                        .map_err(|_| error(line, format!("invalid hex byte {text:?}")))?;
                    decoded.push(byte);
                }
                tokens.push(Spanned {
                    token: Token::HexBytes(decoded),
                    line,
                });
            }
            b'"' => {
                i += 1;
                let mut text = String::new();
                loop {
                    let Some(&c) = bytes.get(i) else {
                        return Err(error(line, "unterminated string literal"));
                    };
                    i += 1;
                    match c {
                        b'"' => break,
                        b'\\' => {
                            let Some(&escape) = bytes.get(i) else {
                                return Err(error(line, "unterminated string escape"));
                            };
                            i += 1;
                            match escape {
                                b'"' => text.push('"'),
                                b'\\' => text.push('\\'),
                                b'0' => text.push('\0'),
                                b'n' => text.push('\n'),
                                _ => {
                                    return Err(error(
                                        line,
                                        format!("unknown string escape \\{}", escape as char),
                                    ));
                                }
                            }
                        }
                        b'\n' => return Err(error(line, "newline in string literal")),
                        _ => {
                            // Continue a UTF-8 sequence byte by byte.
                            let mut buffer = vec![c];
                            while i < bytes.len() && bytes[i] & 0xc0 == 0x80 {
                                buffer.push(bytes[i]);
                                i += 1;
                            }
                            let chunk = std::str::from_utf8(&buffer)
                                .map_err(|_| error(line, "invalid UTF-8 in string literal"))?;
                            text.push_str(chunk);
                        }
                    }
                }
                tokens.push(Spanned {
                    token: Token::Str(text),
                    line,
                });
            }
            _ if c.is_ascii_alphabetic() || c == b'_' => {
                let start = i;
                while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                    i += 1;
                }
                tokens.push(Spanned {
                    token: Token::Ident(input[start..i].to_owned()),
                    line,
                });
            }
            _ => {
                return Err(error(line, format!("unexpected character {:?}", c as char)));
            }
        }
    }
    Ok(tokens)
}

fn error(line: usize, message: impl Into<String>) -> ParseError {
    ParseError {
        line,
        message: message.into(),
    }
}

struct Parser {
    tokens: Vec<Spanned>,
    position: usize,
}

impl Parser {
    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.position).map(|spanned| &spanned.token)
    }

    fn line(&self) -> usize {
        self.tokens
            .get(self.position.min(self.tokens.len().saturating_sub(1)))
            .map_or(0, |spanned| spanned.line)
    }

    fn next(&mut self) -> Result<Spanned> {
        let spanned = self
            .tokens
            .get(self.position)
            .cloned()
            .ok_or_else(|| error(self.line(), "unexpected end of specification"))?;
        self.position += 1;
        Ok(spanned)
    }

    fn ident(&mut self) -> Result<String> {
        let spanned = self.next()?;
        match spanned.token {
            Token::Ident(name) => Ok(name),
            other => Err(error(spanned.line, format!("expected name, got {other:?}"))),
        }
    }

    fn string(&mut self) -> Result<String> {
        let spanned = self.next()?;
        match spanned.token {
            Token::Str(text) => Ok(text),
            other => Err(error(
                spanned.line,
                format!("expected string, got {other:?}"),
            )),
        }
    }

    fn hex_bytes(&mut self) -> Result<Vec<u8>> {
        let spanned = self.next()?;
        match spanned.token {
            Token::HexBytes(bytes) => Ok(bytes),
            other => Err(error(
                spanned.line,
                format!("expected x\"..\" byte literal, got {other:?}"),
            )),
        }
    }

    fn number(&mut self) -> Result<String> {
        let spanned = self.next()?;
        match spanned.token {
            Token::Number(text) => Ok(text),
            other => Err(error(
                spanned.line,
                format!("expected number, got {other:?}"),
            )),
        }
    }

    fn expect(&mut self, token: Token) -> Result<()> {
        let spanned = self.next()?;
        if spanned.token == token {
            Ok(())
        } else {
            Err(error(
                spanned.line,
                format!("expected {token:?}, got {:?}", spanned.token),
            ))
        }
    }

    fn keyword(&mut self, word: &str) -> Result<()> {
        let spanned = self.next()?;
        match spanned.token {
            Token::Ident(name) if name == word => Ok(()),
            other => Err(error(
                spanned.line,
                format!("expected {word:?}, got {other:?}"),
            )),
        }
    }

    fn at_keyword(&self, word: &str) -> bool {
        matches!(self.peek(), Some(Token::Ident(name)) if name == word)
    }

    fn optional_string(&mut self) -> Result<Option<String>> {
        if matches!(self.peek(), Some(Token::Str(_))) {
            Ok(Some(self.string()?))
        } else {
            Ok(None)
        }
    }
}

pub fn parse(input: &str) -> Result<Spec> {
    let mut parser = Parser {
        tokens: lex(input)?,
        position: 0,
    };
    parser.keyword("format")?;
    let name = parser.ident()?;
    let version_word = parser.ident()?;
    let version = version_word
        .strip_prefix('v')
        .and_then(|digits| digits.parse::<u32>().ok())
        .ok_or_else(|| {
            error(
                parser.line(),
                format!("expected version like v0, got {version_word:?}"),
            )
        })?;

    let mut spec = Spec {
        name,
        version,
        domains: Vec::new(),
        codecs: Vec::new(),
        records: Vec::new(),
        fixtures: Vec::new(),
        keyspaces: Vec::new(),
    };

    while parser.peek().is_some() {
        let keyword = parser.ident()?;
        match keyword.as_str() {
            "domain" => spec.domains.push(parse_domain(&mut parser)?),
            "codec" => spec.codecs.push(parse_codec(&mut parser)?),
            "record" => spec.records.push(parse_record(&mut parser)?),
            "fixture" => spec.fixtures.push(parse_fixture(&mut parser)?),
            "keyspace" => spec.keyspaces.push(parse_keyspace(&mut parser)?),
            other => {
                return Err(error(
                    parser.line(),
                    format!("unknown top-level declaration {other:?}"),
                ));
            }
        }
    }
    Ok(spec)
}

fn parse_domain(parser: &mut Parser) -> Result<Domain> {
    let name = parser.ident()?;
    parser.expect(Token::LBrace)?;
    let mut domain = Domain {
        name,
        comparator: None,
        cases: Vec::new(),
    };
    loop {
        if matches!(parser.peek(), Some(Token::RBrace)) {
            parser.next()?;
            break;
        }
        let keyword = parser.ident()?;
        match keyword.as_str() {
            "comparator" => domain.comparator = Some(parser.string()?),
            "cases" => {
                while matches!(parser.peek(), Some(Token::Ident(_))) {
                    domain.cases.push(parser.ident()?);
                }
            }
            other => {
                return Err(error(
                    parser.line(),
                    format!("unknown domain item {other:?}"),
                ));
            }
        }
    }
    Ok(domain)
}

fn parse_kernel(parser: &mut Parser) -> Result<Kernel> {
    let role = parser.ident()?;
    let path = parser.string()?;
    let convention = if parser.at_keyword("convention") {
        parser.keyword("convention")?;
        Some(parser.ident()?)
    } else {
        None
    };
    Ok(Kernel {
        role,
        path,
        convention,
    })
}

fn parse_law(parser: &mut Parser) -> Result<Law> {
    let name = parser.ident()?;
    let comparator = if parser.at_keyword("comparator") {
        parser.keyword("comparator")?;
        Some(parser.ident()?)
    } else {
        None
    };
    Ok(Law { name, comparator })
}

fn parse_codec(parser: &mut Parser) -> Result<Codec> {
    let name = parser.ident()?;
    parser.expect(Token::LBrace)?;
    let mut codec = Codec {
        name,
        value: None,
        refines: None,
        kernels: Vec::new(),
        laws: Vec::new(),
        notes: Vec::new(),
        rules: Vec::new(),
        cases: Vec::new(),
        vectors: Vec::new(),
    };
    loop {
        if matches!(parser.peek(), Some(Token::RBrace)) {
            parser.next()?;
            break;
        }
        let keyword = parser.ident()?;
        match keyword.as_str() {
            "value" => codec.value = Some(parser.string()?),
            "refines" => codec.refines = Some(parser.ident()?),
            "kernel" => codec.kernels.push(parse_kernel(parser)?),
            "law" => codec.laws.push(parse_law(parser)?),
            "note" => codec.notes.push(parser.string()?),
            "rule" => codec.rules.push(parser.string()?),
            "case" => codec.cases.push(parse_case(parser)?),
            "vectors" => codec.vectors.extend(parse_vectors(parser)?),
            other => {
                return Err(error(
                    parser.line(),
                    format!("unknown codec item {other:?}"),
                ));
            }
        }
    }
    Ok(codec)
}

fn parse_case(parser: &mut Parser) -> Result<Case> {
    let name = parser.ident()?;
    parser.expect(Token::LBrace)?;
    let mut case = Case {
        name,
        tag: None,
        const_name: None,
        encoder: None,
        payload: None,
        escape: None,
        terminator: None,
    };
    loop {
        if matches!(parser.peek(), Some(Token::RBrace)) {
            parser.next()?;
            break;
        }
        let keyword = parser.ident()?;
        match keyword.as_str() {
            "tag" => {
                let bytes = parser.hex_bytes()?;
                if bytes.len() != 1 {
                    return Err(error(parser.line(), "case tag must be one byte"));
                }
                case.tag = Some(bytes[0]);
            }
            "const" => case.const_name = Some(parser.string()?),
            "encoder" => {
                let name = parser.string()?;
                parser.keyword("form")?;
                let form = match parser.ident()?.as_str() {
                    "array" => EncoderForm::Array,
                    "option_array" => EncoderForm::OptionArray,
                    "vec" => EncoderForm::Vec,
                    other => {
                        return Err(error(
                            parser.line(),
                            format!("unknown encoder form {other:?}"),
                        ));
                    }
                };
                case.encoder = Some(Encoder { name, form });
            }
            "payload" => case.payload = Some(parser.string()?),
            "escape" => {
                let from = parser.hex_bytes()?;
                parser.expect(Token::Arrow)?;
                let to = parser.hex_bytes()?;
                case.escape = Some((from, to));
            }
            "terminator" => case.terminator = Some(parser.hex_bytes()?),
            other => {
                return Err(error(parser.line(), format!("unknown case item {other:?}")));
            }
        }
    }
    Ok(case)
}

fn parse_value(parser: &mut Parser) -> Result<SpecValue> {
    let keyword = parser.ident()?;
    match keyword.as_str() {
        "null" => Ok(SpecValue::Null),
        "true" => Ok(SpecValue::Bool(true)),
        "false" => Ok(SpecValue::Bool(false)),
        "int" => {
            let text = parser.number()?;
            let value = text
                .parse::<i128>()
                .map_err(|_| error(parser.line(), format!("invalid integer literal {text:?}")))?;
            Ok(SpecValue::Int(value))
        }
        "float" => {
            let text = parser.number()?;
            text.parse::<f64>()
                .map_err(|_| error(parser.line(), format!("invalid float64 literal {text:?}")))?;
            Ok(SpecValue::Float(text))
        }
        "text" => Ok(SpecValue::Text(parser.string()?)),
        "row" => {
            parser.expect(Token::LBrace)?;
            let mut fields = Vec::new();
            loop {
                if matches!(parser.peek(), Some(Token::RBrace)) {
                    parser.next()?;
                    break;
                }
                if !fields.is_empty() {
                    parser.expect(Token::Comma)?;
                    if matches!(parser.peek(), Some(Token::RBrace)) {
                        parser.next()?;
                        break;
                    }
                }
                let name = parser.ident()?;
                parser.expect(Token::Eq)?;
                fields.push((name, parse_value(parser)?));
            }
            Ok(SpecValue::Row(fields))
        }
        other => Err(error(
            parser.line(),
            format!("unknown value keyword {other:?}"),
        )),
    }
}

fn parse_vectors(parser: &mut Parser) -> Result<Vec<Vector>> {
    parser.expect(Token::LBrace)?;
    let mut vectors = Vec::new();
    loop {
        if matches!(parser.peek(), Some(Token::RBrace)) {
            parser.next()?;
            break;
        }
        let keyword = parser.ident()?;
        match keyword.as_str() {
            "accept" => {
                let bytes = parser.hex_bytes()?;
                parser.expect(Token::Eq)?;
                let value = parse_value(parser)?;
                let note = parser.optional_string()?;
                vectors.push(Vector::Accept { bytes, value, note });
            }
            "reject" => {
                let bytes = parser.hex_bytes()?;
                let note = parser.string()?;
                vectors.push(Vector::Reject { bytes, note });
            }
            "order" => {
                let mut values = vec![parse_value(parser)?];
                while matches!(parser.peek(), Some(Token::Lt)) {
                    parser.next()?;
                    values.push(parse_value(parser)?);
                }
                if values.len() < 2 {
                    return Err(error(
                        parser.line(),
                        "order vector needs at least two values",
                    ));
                }
                vectors.push(Vector::Order { values });
            }
            "bound" => {
                let prefix = parser.hex_bytes()?;
                parser.expect(Token::Arrow)?;
                let end = if parser.at_keyword("none") {
                    parser.keyword("none")?;
                    None
                } else {
                    Some(parser.hex_bytes()?)
                };
                vectors.push(Vector::Bound { prefix, end });
            }
            other => {
                return Err(error(
                    parser.line(),
                    format!("unknown vector kind {other:?}"),
                ));
            }
        }
    }
    Ok(vectors)
}

fn parse_record(parser: &mut Parser) -> Result<Record> {
    let name = parser.ident()?;
    parser.expect(Token::LBrace)?;
    let mut record = Record {
        name,
        items: Vec::new(),
        kernels: Vec::new(),
        laws: Vec::new(),
        fixture: None,
        notes: Vec::new(),
        rules: Vec::new(),
        vectors: Vec::new(),
    };
    loop {
        if matches!(parser.peek(), Some(Token::RBrace)) {
            parser.next()?;
            break;
        }
        let keyword = parser.ident()?;
        match keyword.as_str() {
            "kernel" => record.kernels.push(parse_kernel(parser)?),
            "law" => record.laws.push(parse_law(parser)?),
            "fixture" => record.fixture = Some(parser.ident()?),
            "note" => record.notes.push(parser.string()?),
            "rule" => record.rules.push(parser.string()?),
            "vectors" => record.vectors.extend(parse_vectors(parser)?),
            other => record.items.push(parse_record_item(parser, other)?),
        }
    }
    Ok(record)
}

fn parse_record_item(parser: &mut Parser, keyword: &str) -> Result<RecordItem> {
    match keyword {
        "magic" => Ok(RecordItem::Magic(parser.hex_bytes()?)),
        "field" => {
            let name = parser.ident()?;
            let codec = parser.ident()?;
            let doc = parser.optional_string()?;
            Ok(RecordItem::Field { name, codec, doc })
        }
        "let" => {
            let name = parser.ident()?;
            Ok(RecordItem::Let {
                name,
                expr: parser.string()?,
            })
        }
        "guard" => Ok(RecordItem::Guard(parser.string()?)),
        "bound" => Ok(RecordItem::Bound(parser.string()?)),
        "when" => {
            let condition = parser.string()?;
            parser.expect(Token::LBrace)?;
            let mut items = Vec::new();
            loop {
                if matches!(parser.peek(), Some(Token::RBrace)) {
                    parser.next()?;
                    break;
                }
                let keyword = parser.ident()?;
                items.push(parse_record_item(parser, &keyword)?);
            }
            Ok(RecordItem::When { condition, items })
        }
        "repeat" => {
            let name = parser.ident()?;
            let count = parser.ident()?;
            parser.expect(Token::LBrace)?;
            let mut items = Vec::new();
            loop {
                if matches!(parser.peek(), Some(Token::RBrace)) {
                    parser.next()?;
                    break;
                }
                let keyword = parser.ident()?;
                items.push(parse_record_item(parser, &keyword)?);
            }
            Ok(RecordItem::Repeat { name, count, items })
        }
        "eof" => Ok(RecordItem::Eof),
        other => Err(error(
            parser.line(),
            format!("unknown record item {other:?}"),
        )),
    }
}

fn parse_fixture(parser: &mut Parser) -> Result<Fixture> {
    parser.keyword("table")?;
    let name = parser.ident()?;
    parser.expect(Token::LBrace)?;
    let mut columns = Vec::new();
    loop {
        if matches!(parser.peek(), Some(Token::RBrace)) {
            parser.next()?;
            break;
        }
        parser.keyword("column")?;
        let id = parser.ident()?;
        let schema_id = parser
            .number()?
            .parse::<u32>()
            .map_err(|_| error(parser.line(), "fixture column schema id must be a u32"))?;
        let column_name = parser.string()?;
        let scalar = parser.ident()?;
        columns.push(FixtureColumn {
            id,
            schema_id,
            column_name,
            scalar,
        });
    }
    Ok(Fixture { name, columns })
}

fn parse_keyspace(parser: &mut Parser) -> Result<Keyspace> {
    let name = parser.ident()?;
    parser.expect(Token::LBrace)?;
    let mut keyspace = Keyspace {
        name,
        magic: Vec::new(),
        spaces: Vec::new(),
        reserved: Vec::new(),
        scan: None,
    };
    loop {
        if matches!(parser.peek(), Some(Token::RBrace)) {
            parser.next()?;
            break;
        }
        let keyword = parser.ident()?;
        match keyword.as_str() {
            "magic" => keyspace.magic = parser.hex_bytes()?,
            "space" => keyspace.spaces.push(parse_space(parser)?),
            "reserved" => {
                let tag = parse_tag(parser)?;
                let reason = parser.string()?;
                keyspace.reserved.push((tag, reason));
            }
            "scan" => keyspace.scan = Some(parse_scan(parser)?),
            other => {
                return Err(error(
                    parser.line(),
                    format!("unknown keyspace item {other:?}"),
                ));
            }
        }
    }
    Ok(keyspace)
}

fn parse_tag(parser: &mut Parser) -> Result<u8> {
    let bytes = parser.hex_bytes()?;
    if bytes.len() != 1 {
        return Err(error(parser.line(), "keyspace tags are one byte"));
    }
    Ok(bytes[0])
}

fn parse_space(parser: &mut Parser) -> Result<Space> {
    let name = parser.ident()?;
    parser.expect(Token::LBrace)?;
    let mut space = Space {
        name,
        tag: 0,
        fields: Vec::new(),
        value: ValueForm::Opaque(String::new()),
        invariants: Vec::new(),
        notes: Vec::new(),
        vectors: Vec::new(),
    };
    loop {
        if matches!(parser.peek(), Some(Token::RBrace)) {
            parser.next()?;
            break;
        }
        let keyword = parser.ident()?;
        match keyword.as_str() {
            "tag" => space.tag = parse_tag(parser)?,
            "key" => {
                parser.expect(Token::LBrace)?;
                loop {
                    if matches!(parser.peek(), Some(Token::RBrace)) {
                        parser.next()?;
                        break;
                    }
                    let name = parser.ident()?;
                    let kind = match parser.ident()?.as_str() {
                        "physical_id" => SegmentKind::PhysicalId(parser.string()?),
                        "uvarint" => SegmentKind::Uvarint,
                        "be32" => SegmentKind::Be32,
                        "be64" => SegmentKind::Be64,
                        "tuple" => SegmentKind::Tuple,
                        "len_bytes" => SegmentKind::LenBytes,
                        "text" => SegmentKind::Text,
                        "bytes" => SegmentKind::Bytes,
                        other => {
                            return Err(error(
                                parser.line(),
                                format!("unknown key segment kind {other:?}"),
                            ));
                        }
                    };
                    space.fields.push(KeyField { name, kind });
                }
            }
            "value" => {
                space.value = if parser.at_keyword("record") {
                    parser.keyword("record")?;
                    ValueForm::Record(parser.ident()?)
                } else if parser.at_keyword("json") {
                    parser.keyword("json")?;
                    parser.keyword("schema")?;
                    ValueForm::JsonSchema(parser.string()?)
                } else if parser.at_keyword("codec") {
                    parser.keyword("codec")?;
                    ValueForm::Codec(parser.ident()?)
                } else {
                    ValueForm::Opaque(parser.string()?)
                };
            }
            "invariant" => space.invariants.push(parser.string()?),
            "note" => space.notes.push(parser.string()?),
            "vectors" => {
                parser.expect(Token::LBrace)?;
                loop {
                    if matches!(parser.peek(), Some(Token::RBrace)) {
                        parser.next()?;
                        break;
                    }
                    parser.keyword("key")?;
                    space.vectors.push(parse_key_vector(parser)?);
                }
            }
            other => {
                return Err(error(
                    parser.line(),
                    format!("unknown space item {other:?}"),
                ));
            }
        }
    }
    Ok(space)
}

fn parse_key_vector(parser: &mut Parser) -> Result<KeyVector> {
    parser.expect(Token::LBrace)?;
    let mut fields = Vec::new();
    loop {
        if matches!(parser.peek(), Some(Token::RBrace)) {
            parser.next()?;
            break;
        }
        if !fields.is_empty() {
            parser.expect(Token::Comma)?;
            if matches!(parser.peek(), Some(Token::RBrace)) {
                parser.next()?;
                break;
            }
        }
        let name = parser.ident()?;
        let value = match parser.peek() {
            Some(Token::Number(_)) => {
                let text = parser.number()?;
                let value = text.parse::<u64>().map_err(|_| {
                    error(parser.line(), format!("invalid key field number {text:?}"))
                })?;
                KeyFieldValue::Number(value)
            }
            Some(Token::HexBytes(_)) => KeyFieldValue::Bytes(parser.hex_bytes()?),
            Some(Token::Str(_)) => KeyFieldValue::Text(parser.string()?),
            other => {
                return Err(error(
                    parser.line(),
                    format!("expected key field value, got {other:?}"),
                ));
            }
        };
        fields.push((name, value));
    }
    parser.expect(Token::Eq)?;
    let bytes = parser.hex_bytes()?;
    let note = parser.optional_string()?;
    Ok(KeyVector {
        fields,
        bytes,
        note,
    })
}

fn parse_scan(parser: &mut Parser) -> Result<Scan> {
    parser.expect(Token::LBrace)?;
    let mut kernel = String::new();
    let mut law = String::new();
    let mut bounds = Vec::new();
    loop {
        if matches!(parser.peek(), Some(Token::RBrace)) {
            parser.next()?;
            break;
        }
        let keyword = parser.ident()?;
        match keyword.as_str() {
            "kernel" => kernel = parser.string()?,
            "law" => law = parser.ident()?,
            "bound" => {
                let prefix = parser.hex_bytes()?;
                parser.expect(Token::Arrow)?;
                let end = if parser.at_keyword("none") {
                    parser.keyword("none")?;
                    None
                } else {
                    Some(parser.hex_bytes()?)
                };
                bounds.push(Vector::Bound { prefix, end });
            }
            other => {
                return Err(error(parser.line(), format!("unknown scan item {other:?}")));
            }
        }
    }
    Ok(Scan {
        kernel,
        law,
        bounds,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_minimal_specification() {
        let spec = parse(
            r#"
            format storage v0

            domain scalar {
              comparator "rad::engine::lir::Value::compare"
              cases null bool int64 float64 text
            }

            codec uvarint {
              value "u64"
              kernel encode "a::b" convention append
              law round_trip
              vectors {
                accept x"00" = int 0
                reject x"80 00" "non-canonical"
                order int -1 < int 0 < int 1
              }
            }

            fixture table fuzz_rows {
              column c1 1 "id" text
            }

            keyspace rad {
              magic x"72 37"
              space data {
                tag x"01"
                key {
                  table physical_id "t"
                  generation uvarint
                  primary_key tuple
                }
                value record row_body
                vectors {
                  key { table 42, generation 1, primary_key x"05 61 00 01" } = x"72 37 01 2a 01 05 61 00 01"
                }
              }
              space names {
                tag x"02"
                key { name text }
                value "table id text"
              }
              reserved x"00" "zero guard"
              scan {
                kernel "a::prefix_end"
                law smallest_exclusive_bound
                bound x"01" -> x"02"
                bound x"ff" -> none
              }
            }
            "#,
        )
        .expect("minimal spec parses");
        assert_eq!(spec.version, 0);
        assert_eq!(spec.domains[0].cases.len(), 5);
        let codec = spec.codec("uvarint").unwrap();
        assert_eq!(codec.kernels[0].convention.as_deref(), Some("append"));
        assert_eq!(codec.vectors.len(), 3);
        assert_eq!(spec.fixtures[0].columns[0].schema_id, 1);
        let keyspace = &spec.keyspaces[0];
        assert_eq!(keyspace.magic, vec![0x72, 0x37]);
        assert_eq!(keyspace.spaces[0].tag, 0x01);
        assert_eq!(keyspace.spaces[0].fields.len(), 3);
        assert_eq!(
            keyspace.spaces[0].fields[0].kind,
            SegmentKind::PhysicalId("t".into())
        );
        assert_eq!(keyspace.spaces[0].vectors.len(), 1);
        assert_eq!(keyspace.reserved, vec![(0x00, "zero guard".into())]);
        assert_eq!(keyspace.scan.as_ref().unwrap().bounds.len(), 2);
    }

    #[test]
    fn reports_the_line_of_a_lex_error() {
        let error = parse("format storage v0\n\n%").expect_err("% does not lex");
        assert_eq!(error.line, 3);
        assert_eq!(error.to_string(), "line 3: unexpected character '%'");
    }

    #[test]
    fn lexes_a_trailing_comment_without_a_newline() {
        parse("format storage v0 # trailing comment").expect("trailing comment lexes");
    }

    #[test]
    fn rejects_malformed_input() {
        for (input, needle) in [
            ("format storage vx", "expected version"),
            ("format storage v0 codec c { tag }", "unknown codec item"),
            (
                r#"format storage v0 codec c { vectors { accept x"0" = int 0 } }"#,
                "even digit",
            ),
            ("format storage v0 wobble x {}", "unknown top-level"),
            (
                r#"format storage v0 codec c { vectors { order int 1 } }"#,
                "at least two",
            ),
        ] {
            let error = parse(input).expect_err(input);
            assert!(error.message.contains(needle), "{input:?} produced {error}");
        }
    }
}
