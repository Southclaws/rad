use std::collections::HashMap;
use std::fmt::Write as _;

use crate::engine::catalog;
use crate::engine::catalog::model::Table;
use crate::engine::exec::{codec, key_describe};
use crate::engine::kv::{IsolationLevel, TransactionView, TransactionalKv, keys};
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

    /// Renders a durable key through the generated parser, with catalog
    /// names added where the catalog still knows the identity.
    pub(super) fn key(&self, key: &[u8]) -> String {
        if let Some(parts) = keys::decode_data_key(key) {
            return format!(
                "data/table={}/generation={}/primary_key={}",
                self.table_label(parts.table),
                parts.generation,
                render_tuple(&parts.primary_key)
            );
        }
        if let Some(parts) = keys::decode_index_key(key) {
            return format!(
                "index/table={}/index={}/{}",
                self.table_label(parts.table),
                self.index_label(parts.table, parts.index),
                self.render_index_tuple(parts.table, parts.index, &parts.tuple_rest)
            );
        }
        if let Some(text) = key_describe::describe_key(key) {
            return text;
        }
        printable(key)
    }

    pub(super) fn value(&self, key: &[u8], value: &[u8]) -> String {
        if keys::decode_index_key(key).is_some() {
            return render_tuple(value);
        }
        if let Some(parts) = keys::decode_data_key(key) {
            if let Some(table) = self.tables.get(&format!("t{}", parts.table))
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

    fn table_label(&self, table: u64) -> String {
        let table_id = format!("t{table}");
        self.tables.get(&table_id).map_or_else(
            || table_id.clone(),
            |table| format!("{table_id}[{}]", table.name),
        )
    }

    fn index_label(&self, table: u64, index: u64) -> String {
        let index_id = format!("i{index}");
        self.tables
            .get(&format!("t{table}"))
            .and_then(|table| {
                table
                    .indexes
                    .iter()
                    .find(|index| index.id.as_str() == index_id)
            })
            .map_or_else(
                || index_id.clone(),
                |index| format!("{index_id}[{}]", index.name),
            )
    }

    fn render_index_tuple(&self, table: u64, index: u64, bytes: &[u8]) -> String {
        let index_id = format!("i{index}");
        let Some(index) = self.tables.get(&format!("t{table}")).and_then(|table| {
            table
                .indexes
                .iter()
                .find(|index| index.id.as_str() == index_id)
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
