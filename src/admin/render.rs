use std::collections::HashMap;
use std::fmt::Write as _;

use crate::engine::catalog;
use crate::engine::catalog::model::Table;
use crate::engine::exec::codec;
use crate::engine::kv::{IsolationLevel, TransactionView, TransactionalKv};
use crate::engine::lir::Value;

#[derive(Default)]
pub(super) struct KeyDecoder {
    tables: HashMap<String, Table>,
}

impl KeyDecoder {
    pub(super) async fn load(store: &dyn TransactionalKv) -> Self {
        let Ok(transaction) = store.begin(IsolationLevel::Snapshot).await else {
            return Self::default();
        };
        let tables = {
            let mut view = TransactionView(transaction.as_ref());
            catalog::store::list_tables(&mut view)
                .await
                .unwrap_or_default()
        };
        transaction.rollback();
        Self {
            tables: tables
                .into_iter()
                .map(|table| (table.id.to_string(), table))
                .collect(),
        }
    }

    pub(super) fn key(&self, key: &[u8]) -> String {
        if let Some(rest) = key.strip_prefix(b"/rad/data/")
            && let Some((table_id, tuple)) = split_once(rest, b"/primary/")
            && let Ok(table_id) = std::str::from_utf8(table_id)
        {
            return format!(
                "/rad/data/{}/primary/{}",
                self.table_label(table_id),
                render_tuple(tuple)
            );
        }
        if let Some(rest) = key.strip_prefix(b"/rad/index/")
            && let Some((table_id, rest)) = split_once(rest, b"/")
            && let Some((index_id, tuple)) = split_once(rest, b"/")
            && let (Ok(table_id), Ok(index_id)) =
                (std::str::from_utf8(table_id), std::str::from_utf8(index_id))
        {
            return format!(
                "/rad/index/{}/{}/{}",
                self.table_label(table_id),
                self.index_label(table_id, index_id),
                self.render_index_tuple(table_id, index_id, tuple)
            );
        }
        printable(key)
    }

    pub(super) fn value(&self, key: &[u8], value: &[u8]) -> String {
        if key.starts_with(b"/rad/index/") {
            return render_tuple(value);
        }
        if let Some(rest) = key.strip_prefix(b"/rad/data/") {
            if let Some((table_id, _)) = split_once(rest, b"/primary/")
                && let Ok(table_id) = std::str::from_utf8(table_id)
                && let Some(table) = self.tables.get(table_id)
                && let Ok(row) = codec::unmarshal_row(table, value)
            {
                let fields = table
                    .columns
                    .iter()
                    .filter_map(|column| {
                        row.get(&column.name)
                            .map(|value| format!("{}: {value}", column.name))
                    })
                    .collect::<Vec<_>>();
                return format!("{{{}}}", fields.join(", "));
            }
            return printable(value);
        }
        if let Ok(json) = serde_json::from_slice::<serde_json::Value>(value)
            && (json.is_object() || json.is_array())
        {
            return json.to_string();
        }
        printable(value)
    }

    fn table_label(&self, table_id: &str) -> String {
        self.tables.get(table_id).map_or_else(
            || table_id.to_owned(),
            |table| format!("{table_id}[{}]", table.name),
        )
    }

    fn index_label(&self, table_id: &str, index_id: &str) -> String {
        self.tables
            .get(table_id)
            .and_then(|table| {
                table
                    .indexes
                    .iter()
                    .find(|index| index.id.to_string() == index_id)
            })
            .map_or_else(
                || index_id.to_owned(),
                |index| format!("{index_id}[{}]", index.name),
            )
    }

    fn render_index_tuple(&self, table_id: &str, index_id: &str, bytes: &[u8]) -> String {
        let Some(index) = self.tables.get(table_id).and_then(|table| {
            table
                .indexes
                .iter()
                .find(|index| index.id.to_string() == index_id)
        }) else {
            return render_tuple(bytes);
        };
        let mut indexed = Vec::with_capacity(index.columns.len());
        let mut rest = bytes;
        for _ in &index.columns {
            let Ok((value, consumed)) = codec::decode_value(rest) else {
                return render_tuple(bytes);
            };
            indexed.push(value);
            rest = &rest[consumed..];
        }
        let indexed = render_values(&indexed);
        if rest.is_empty() {
            indexed
        } else {
            format!("{indexed}+{}", render_tuple(rest))
        }
    }
}

fn split_once<'a>(value: &'a [u8], delimiter: &[u8]) -> Option<(&'a [u8], &'a [u8])> {
    let position = value
        .windows(delimiter.len())
        .position(|window| window == delimiter)?;
    Some((&value[..position], &value[position + delimiter.len()..]))
}

fn render_tuple(bytes: &[u8]) -> String {
    if bytes.is_empty() {
        return "()".into();
    }
    codec::decode_tuple(bytes)
        .map(|values| render_values(&values))
        .unwrap_or_else(|_| printable(bytes))
}

fn render_values(values: &[Value]) -> String {
    format!(
        "({})",
        values
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(", ")
    )
}

fn printable(bytes: &[u8]) -> String {
    let mut output = String::new();
    for byte in bytes {
        if byte.is_ascii_graphic() || *byte == b' ' {
            output.push(char::from(*byte));
        } else {
            write!(&mut output, "\\x{byte:02x}").expect("writing to a String cannot fail");
        }
    }
    output
}

pub(super) fn hex_dump(bytes: &[u8]) -> String {
    let mut output = String::new();
    for (offset, chunk) in bytes.chunks(16).enumerate() {
        write!(&mut output, "{:08x}  ", offset * 16).expect("writing to a String cannot fail");
        for index in 0..16 {
            if let Some(byte) = chunk.get(index) {
                write!(&mut output, "{byte:02x} ").expect("writing to a String cannot fail");
            } else {
                output.push_str("   ");
            }
            if index == 7 {
                output.push(' ');
            }
        }
        output.push_str(" |");
        for byte in chunk {
            output.push(if byte.is_ascii_graphic() || *byte == b' ' {
                char::from(*byte)
            } else {
                '.'
            });
        }
        output.push_str("|\n");
    }
    output
}
