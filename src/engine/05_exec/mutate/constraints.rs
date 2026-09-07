use bytes::Bytes;
use tracing::Instrument as _;

use crate::engine::catalog::model::{ForeignKey, Index, Table};
use crate::engine::catalog::store;
use crate::engine::kv::key_encoding::prefix_end;
use crate::engine::kv::{KeyRange, KvView};
use crate::engine::lir::{Row, Value};

use super::super::row_store;
use super::super::{Error, ErrorKind, Result, codec};

pub(super) async fn check_foreign_keys(
    view: &mut dyn KvView,
    table: &Table,
    row: &Row,
) -> Result<()> {
    for foreign_key in &table.foreign_keys {
        check_foreign_key(view, row, foreign_key).await?;
    }
    Ok(())
}

pub(super) async fn check_foreign_keys_for(
    view: &mut dyn KvView,
    table: &Table,
    row: &Row,
    assigned: &Row,
) -> Result<()> {
    for foreign_key in &table.foreign_keys {
        if touches(&foreign_key.columns, assigned) {
            check_foreign_key(view, row, foreign_key).await?;
        }
    }
    Ok(())
}

async fn check_foreign_key(
    view: &mut dyn KvView,
    row: &Row,
    foreign_key: &ForeignKey,
) -> Result<()> {
    let values = foreign_key
        .columns
        .iter()
        .map(|column| {
            row.get(column).cloned().ok_or_else(|| {
                Error::message(
                    ErrorKind::Internal,
                    format!("exec: normalized row lacks foreign-key column {column:?}"),
                )
            })
        })
        .collect::<Result<Vec<_>>>()?;
    if values.iter().any(Value::is_null) {
        return Ok(());
    }
    let tuple = codec::encode_tuple(&values)?;
    let referenced = store::get_table_by_id(view, &foreign_key.ref_table_id)
        .await
        .map_err(Error::from)?
        .ok_or_else(|| {
            Error::message(
                ErrorKind::CorruptData,
                format!(
                    "exec: foreign key {:?} references missing table {:?}",
                    foreign_key.name, foreign_key.ref_table_id
                ),
            )
        })?;
    let key = codec::data_key(&referenced, &tuple)?;
    if view.get(&key).await?.is_none() {
        return Err(Error::message(
            ErrorKind::ConstraintViolation,
            format!(
                "exec: foreign key {:?} violation: referenced row does not exist",
                foreign_key.name
            ),
        ));
    }
    Ok(())
}

pub(super) async fn check_unique_indexes(
    view: &mut dyn KvView,
    table: &Table,
    row: &Row,
    primary_key: &[u8],
) -> Result<()> {
    for index in table.indexes.iter().filter(|index| index.is_ready()) {
        check_unique_index(view, table, row, primary_key, index).await?;
    }
    Ok(())
}

pub(super) async fn check_unique_indexes_for(
    view: &mut dyn KvView,
    table: &Table,
    row: &Row,
    primary_key: &[u8],
    assigned: &Row,
) -> Result<()> {
    for index in table.indexes.iter().filter(|index| index.is_ready()) {
        let columns = index_columns(table, index);
        if touches(&columns, assigned) {
            check_unique_index(view, table, row, primary_key, index).await?;
        }
    }
    Ok(())
}

pub(crate) async fn check_unique_index(
    view: &dyn KvView,
    table: &Table,
    row: &Row,
    primary_key: &[u8],
    index: &Index,
) -> Result<()> {
    if !index.unique {
        return Ok(());
    }
    if codec::index_has_null(table, index, row) {
        return Ok(());
    }
    let tuple = codec::encode_index_tuple(table, index, row)?;
    let mut prefix = codec::index_prefix(table, &index.id)?;
    prefix.extend_from_slice(&tuple);
    let mut iterator = view
        .scan(KeyRange {
            start: Some(Bytes::copy_from_slice(&prefix)),
            end: prefix_end(&prefix).map(Bytes::from),
        })
        .await?;
    while let Some(entry) = iterator.next().await? {
        if entry.value.as_ref() != primary_key {
            return Err(Error::message(
                ErrorKind::ConstraintViolation,
                format!(
                    "exec: unique index {:?} violation in table {:?}",
                    index.name, table.name
                ),
            ));
        }
    }
    Ok(())
}

pub(super) async fn check_no_references(
    view: &mut dyn KvView,
    table: &Table,
    row: &Row,
) -> Result<()> {
    let tables = store::list_tables(view).await?;
    for child in tables {
        for foreign_key in &child.foreign_keys {
            if foreign_key.ref_table_id != table.id {
                continue;
            }
            let mut wanted = Row::new();
            for (child_column, parent_column) in
                foreign_key.columns.iter().zip(&foreign_key.ref_columns)
            {
                let value = row.get(parent_column).cloned().ok_or_else(|| {
                    Error::message(
                        ErrorKind::CorruptData,
                        format!(
                            "exec: foreign key {:?} references missing parent column {:?}",
                            foreign_key.name, parent_column
                        ),
                    )
                })?;
                wanted.insert(child_column.clone(), value);
            }
            if any_row_matching(view, &child, &foreign_key.columns, &wanted).await? {
                return Err(Error::message(
                    ErrorKind::ConstraintViolation,
                    format!(
                        "exec: cannot delete from {:?}: row is referenced by {:?} via {:?}",
                        table.name, child.name, foreign_key.name
                    ),
                ));
            }
        }
    }
    Ok(())
}

async fn any_row_matching(
    view: &mut dyn KvView,
    table: &Table,
    columns: &[String],
    wanted: &Row,
) -> Result<bool> {
    let span = tracing::debug_span!(
        target: "rad::telemetry",
        "rad.constraint.reference_check",
        otel.name = "rad.constraint.reference_check",
        otel.kind = "internal",
        rad.constraint.kind = "foreign_key_reference",
        rad.constraint.table = table.name.as_str(),
        rad.constraint.column_count = columns.len(),
        rad.constraint.access_path = tracing::field::Empty,
        rad.constraint.rows_examined = tracing::field::Empty,
        rad.constraint.match_found = tracing::field::Empty,
        rad.status = tracing::field::Empty,
        otel.status_code = tracing::field::Empty,
    );
    let result = find_any_row_matching(view, table, columns, wanted)
        .instrument(span.clone())
        .await;
    match &result {
        Ok(result) => {
            span.record("rad.constraint.access_path", result.access_path.as_str());
            span.record("rad.constraint.rows_examined", result.rows_examined);
            span.record("rad.constraint.match_found", result.found);
            span.record("rad.status", "success");
        }
        Err(_) => {
            span.record("rad.status", "error");
            span.record("otel.status_code", "ERROR");
        }
    }
    result.map(|result| result.found)
}

async fn find_any_row_matching(
    view: &mut dyn KvView,
    table: &Table,
    columns: &[String],
    wanted: &Row,
) -> Result<ReferenceMatch> {
    if table.primary_key.len() >= columns.len() && table.primary_key[..columns.len()] == *columns {
        let tuple = codec::encode_row_tuple(wanted, columns)?;
        if table.primary_key.len() == columns.len() {
            let found = view.get(&codec::data_key(table, &tuple)?).await?.is_some();
            return Ok(ReferenceMatch {
                found,
                rows_examined: u64::from(found),
                access_path: ReferenceAccessPath::PrimaryKeyGet,
            });
        }
        let mut prefix = codec::data_prefix(table)?;
        prefix.extend_from_slice(&tuple);
        let mut iterator = view
            .scan(KeyRange {
                start: Some(Bytes::copy_from_slice(&prefix)),
                end: prefix_end(&prefix).map(Bytes::from),
            })
            .await?;
        let found = iterator.next().await?.is_some();
        return Ok(ReferenceMatch {
            found,
            rows_examined: u64::from(found),
            access_path: ReferenceAccessPath::PrimaryKeyPrefix,
        });
    }

    for index in table.indexes.iter().filter(|index| index.is_ready()) {
        let index_columns = index_columns(table, index);
        if index_columns.len() < columns.len() || index_columns[..columns.len()] != *columns {
            continue;
        }
        let tuple = codec::encode_row_tuple(wanted, columns)?;
        let mut prefix = codec::index_prefix(table, &index.id)?;
        prefix.extend_from_slice(&tuple);
        let mut iterator = view
            .scan(KeyRange {
                start: Some(Bytes::copy_from_slice(&prefix)),
                end: prefix_end(&prefix).map(Bytes::from),
            })
            .await?;
        let found = iterator.next().await?.is_some();
        return Ok(ReferenceMatch {
            found,
            rows_examined: u64::from(found),
            access_path: ReferenceAccessPath::SecondaryIndexPrefix,
        });
    }

    let mut iterator = row_store::scan_table(view, table, &table.columns).await?;
    let positions = columns
        .iter()
        .map(|column| {
            table
                .columns
                .iter()
                .position(|candidate| candidate.name == *column)
                .expect("constraint column belongs to its table")
        })
        .collect::<Vec<_>>();
    let mut rows_examined = 0_u64;
    while let Some(candidate) = iterator.next().await? {
        rows_examined = rows_examined.saturating_add(1);
        if columns
            .iter()
            .zip(&positions)
            .all(|(column, position)| candidate.get(*position) == wanted.get(column))
        {
            return Ok(ReferenceMatch {
                found: true,
                rows_examined,
                access_path: ReferenceAccessPath::TableScan,
            });
        }
    }
    Ok(ReferenceMatch {
        found: false,
        rows_examined,
        access_path: ReferenceAccessPath::TableScan,
    })
}

struct ReferenceMatch {
    found: bool,
    rows_examined: u64,
    access_path: ReferenceAccessPath,
}

enum ReferenceAccessPath {
    PrimaryKeyGet,
    PrimaryKeyPrefix,
    SecondaryIndexPrefix,
    TableScan,
}

impl ReferenceAccessPath {
    const fn as_str(&self) -> &'static str {
        match self {
            Self::PrimaryKeyGet => "primary_key_get",
            Self::PrimaryKeyPrefix => "primary_key_prefix",
            Self::SecondaryIndexPrefix => "secondary_index_prefix",
            Self::TableScan => "table_scan",
        }
    }
}

fn index_columns(table: &Table, index: &Index) -> Vec<String> {
    table
        .index_column_names(index)
        .into_iter()
        .map(str::to_owned)
        .collect()
}

fn touches(columns: &[String], assigned: &Row) -> bool {
    columns.iter().any(|column| assigned.contains_key(column))
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use async_trait::async_trait;

    use crate::engine::catalog::identity::{
        DefinitionGeneration, ExistenceGeneration, SchemaId, StorageGeneration, ValueGeneration,
        WriteProtocolGeneration,
    };
    use crate::engine::catalog::model::{Column, ScalarType};
    use crate::engine::kv::slatedb::Store;
    use crate::engine::kv::{Kv, KvIterator};

    use super::*;

    struct RecordingView {
        store: Store,
        gets: Mutex<Vec<Vec<u8>>>,
        scans: Mutex<Vec<KeyRange>>,
    }

    #[async_trait]
    impl KvView for RecordingView {
        async fn get(&self, key: &[u8]) -> crate::engine::kv::Result<Option<Bytes>> {
            self.gets
                .lock()
                .expect("recorded get lock poisoned")
                .push(key.to_vec());
            Kv::get(&self.store, key).await
        }

        async fn put(&self, key: Bytes, value: Bytes) -> crate::engine::kv::Result<()> {
            Kv::put(&self.store, key, value).await
        }

        async fn delete(&self, key: &[u8]) -> crate::engine::kv::Result<()> {
            Kv::delete(&self.store, key).await
        }

        async fn scan<'a>(
            &'a self,
            range: KeyRange,
        ) -> crate::engine::kv::Result<Box<dyn KvIterator + 'a>> {
            self.scans
                .lock()
                .expect("recorded scan lock poisoned")
                .push(range.clone());
            Kv::scan(&self.store, range).await
        }
    }

    #[tokio::test]
    async fn composite_primary_key_prefix_limits_reference_scan() {
        let mut view = recording_view("constraint-primary-key-prefix").await;
        let table = order_items_table();
        seed_row(&view, &table, "order-1", "product-1").await;
        seed_row(&view, &table, "order-2", "product-2").await;
        let columns = vec!["item_order_id".into()];
        let wanted = Row::from([("item_order_id".into(), Value::Text("order-1".into()))]);

        assert!(
            any_row_matching(&mut view, &table, &columns, &wanted)
                .await
                .unwrap()
        );

        let tuple = codec::encode_row_tuple(&wanted, &columns).unwrap();
        let mut prefix = codec::data_prefix(&table).unwrap();
        prefix.extend_from_slice(&tuple);
        assert!(view.gets.lock().unwrap().is_empty());
        assert_eq!(
            *view.scans.lock().unwrap(),
            vec![KeyRange {
                start: Some(Bytes::copy_from_slice(&prefix)),
                end: prefix_end(&prefix).map(Bytes::from),
            }]
        );
    }

    #[tokio::test]
    async fn composite_primary_key_prefix_ignores_unrelated_rows() {
        let mut view = recording_view("constraint-primary-key-prefix-empty").await;
        let table = order_items_table();
        seed_row(&view, &table, "order-2", "product-2").await;
        let columns = vec!["item_order_id".into()];
        let wanted = Row::from([("item_order_id".into(), Value::Text("order-1".into()))]);

        assert!(
            !any_row_matching(&mut view, &table, &columns, &wanted)
                .await
                .unwrap()
        );

        let tuple = codec::encode_row_tuple(&wanted, &columns).unwrap();
        let mut prefix = codec::data_prefix(&table).unwrap();
        prefix.extend_from_slice(&tuple);
        assert!(view.gets.lock().unwrap().is_empty());
        assert_eq!(
            *view.scans.lock().unwrap(),
            vec![KeyRange {
                start: Some(Bytes::copy_from_slice(&prefix)),
                end: prefix_end(&prefix).map(Bytes::from),
            }]
        );
    }

    #[tokio::test]
    async fn complete_primary_key_uses_point_get() {
        let mut view = recording_view("constraint-primary-key-get").await;
        let table = order_items_table();
        seed_row(&view, &table, "order-1", "product-1").await;
        let columns = table.primary_key.clone();
        let wanted = Row::from([
            ("item_order_id".into(), Value::Text("order-1".into())),
            ("item_product_id".into(), Value::Text("product-1".into())),
        ]);

        assert!(
            any_row_matching(&mut view, &table, &columns, &wanted)
                .await
                .unwrap()
        );

        let tuple = codec::encode_row_tuple(&wanted, &columns).unwrap();
        assert_eq!(
            *view.gets.lock().unwrap(),
            vec![codec::data_key(&table, &tuple).unwrap()]
        );
        assert!(view.scans.lock().unwrap().is_empty());
    }

    async fn recording_view(name: &str) -> RecordingView {
        RecordingView {
            store: Store::memory(name).await.unwrap(),
            gets: Mutex::new(Vec::new()),
            scans: Mutex::new(Vec::new()),
        }
    }

    async fn seed_row(view: &RecordingView, table: &Table, order: &str, product: &str) {
        let row = Row::from([
            ("item_order_id".into(), Value::Text(order.to_owned())),
            ("item_product_id".into(), Value::Text(product.to_owned())),
        ]);
        let tuple = codec::encode_row_tuple(&row, &table.primary_key).unwrap();
        Kv::put(
            &view.store,
            Bytes::from(codec::data_key(table, &tuple).unwrap()),
            Bytes::from_static(b"row"),
        )
        .await
        .unwrap();
    }

    fn order_items_table() -> Table {
        Table {
            id: "t4".into(),
            schema_id: SchemaId::new(4).unwrap(),
            name: "order_items".into(),
            definition_generation: DefinitionGeneration::ZERO,
            existence_generation: ExistenceGeneration::ZERO,
            write_protocol_generation: WriteProtocolGeneration::ZERO,
            storage_generation: StorageGeneration::INITIAL,
            columns: ["item_order_id", "item_product_id"]
                .into_iter()
                .enumerate()
                .map(|(index, name)| Column {
                    id: format!("c{}", index + 1).into(),
                    schema_id: SchemaId::new(index as u32 + 1).unwrap(),
                    name: name.into(),
                    value_generation: ValueGeneration::ZERO,
                    scalar_type: ScalarType::Text,
                    nullable: false,
                    format: String::new(),
                    insert_default: None,
                    missing_value: None,
                })
                .collect(),
            primary_key: vec!["item_order_id".into(), "item_product_id".into()],
            indexes: Vec::new(),
            foreign_keys: Vec::new(),
            constraints: Vec::new(),
        }
    }
}
