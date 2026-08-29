//! Physical row access over the ordered KV layout.

use async_trait::async_trait;
use bytes::Bytes;

use crate::engine::catalog::model::{Column, Index, Table};
use crate::engine::kv::key_encoding::prefix_end;
use crate::engine::kv::{KeyRange, KvIterator, KvView, ScanRequest};
use crate::engine::lir::{Row, Value};

use super::codec;
use super::{Error, ErrorKind, Result};

pub(super) async fn get_columns(
    view: &dyn KvView,
    table: &Table,
    key: &Row,
    columns: &[Column],
) -> Result<Option<Row>> {
    if key.len() != table.primary_key.len()
        || table
            .primary_key
            .iter()
            .any(|column| !key.contains_key(column))
    {
        return Err(Error::message(
            ErrorKind::Internal,
            format!("exec: incomplete primary key for table {:?}", table.name),
        ));
    }
    let primary_key = codec::encode_row_tuple(key, &table.primary_key)?;
    view.get(&codec::data_key(table, &primary_key)?)
        .await?
        .map(|raw| codec::unmarshal_row_columns(table, columns, &raw))
        .transpose()
}

pub(super) async fn scan_table_columns(
    view: &dyn KvView,
    table: &Table,
    columns: &[Column],
) -> Result<Vec<Row>> {
    collect(scan_table(view, table, columns).await?).await
}

#[derive(Clone, Debug)]
pub(super) struct BatchRow {
    pub key: Vec<u8>,
    pub primary_key: Vec<u8>,
    pub row: Row,
}

#[derive(Clone, Debug)]
pub(super) struct RawBatchRow {
    pub key: Vec<u8>,
    pub primary_key: Vec<u8>,
    pub raw: Vec<u8>,
}

pub(super) async fn scan_table_batch(
    view: &dyn KvView,
    table: &Table,
    cursor: &[u8],
    limit: usize,
) -> Result<Vec<BatchRow>> {
    let raw = scan_raw_table_batch(view, table, cursor, limit).await?;
    raw.into_iter()
        .map(|row| {
            Ok(BatchRow {
                key: row.key,
                primary_key: row.primary_key,
                row: codec::unmarshal_row(table, &row.raw)?,
            })
        })
        .collect()
}

pub(super) async fn scan_raw_table_batch(
    view: &dyn KvView,
    table: &Table,
    cursor: &[u8],
    limit: usize,
) -> Result<Vec<RawBatchRow>> {
    let prefix = codec::data_prefix(table)?;
    let mut start = prefix.clone();
    if !cursor.is_empty() {
        start = cursor.to_vec();
        start.push(0);
    }
    let mut iterator = view
        .scan(KeyRange {
            start: Some(Bytes::from(start)),
            end: prefix_end(&prefix).map(Bytes::from),
        })
        .await?;
    let mut rows = Vec::with_capacity(limit);
    while rows.len() < limit {
        let Some(entry) = iterator.next().await? else {
            break;
        };
        if !entry.key.starts_with(&prefix) {
            return Err(Error::message(
                ErrorKind::CorruptData,
                format!(
                    "exec: raw table scan for {:?} escaped its prefix",
                    table.name
                ),
            ));
        }
        rows.push(RawBatchRow {
            primary_key: entry.key[prefix.len()..].to_vec(),
            key: entry.key.to_vec(),
            raw: entry.value.to_vec(),
        });
    }
    Ok(rows)
}

#[async_trait]
pub(super) trait RowIterator: Send {
    async fn next(&mut self) -> Result<Option<Row>>;
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum ScanPruning {
    Empty,
    Range(KeyRange),
}

pub(super) async fn scan_table<'a>(
    view: &'a dyn KvView,
    table: &Table,
    columns: &[Column],
) -> Result<Box<dyn RowIterator + 'a>> {
    scan_table_with_pruning(view, table, columns, None).await
}

pub(super) async fn scan_table_with_pruning<'a>(
    view: &'a dyn KvView,
    table: &Table,
    columns: &[Column],
    pruning: Option<&ScanPruning>,
) -> Result<Box<dyn RowIterator + 'a>> {
    let prefix = codec::data_prefix(table)?;
    let range = KeyRange {
        start: Some(Bytes::from(prefix.clone())),
        end: prefix_end(&prefix).map(Bytes::from),
    };
    let Some(iterator) = open_scan(view, range, pruning).await? else {
        return Ok(Box::new(EmptyIterator));
    };
    Ok(Box::new(TableIterator {
        iterator,
        prefix,
        table: table.clone(),
        columns: columns.to_vec(),
    }))
}

struct TableIterator<'a> {
    iterator: Box<dyn KvIterator + 'a>,
    prefix: Vec<u8>,
    table: Table,
    columns: Vec<Column>,
}

#[async_trait]
impl RowIterator for TableIterator<'_> {
    async fn next(&mut self) -> Result<Option<Row>> {
        let Some(entry) = self.iterator.next().await? else {
            return Ok(None);
        };
        if !entry.key.starts_with(&self.prefix) {
            return Err(Error::message(
                ErrorKind::CorruptData,
                format!(
                    "exec: table scan for {:?} escaped its prefix",
                    self.table.name
                ),
            ));
        }
        Ok(Some(codec::unmarshal_row_columns(
            &self.table,
            &self.columns,
            &entry.value,
        )?))
    }
}

pub(super) struct Range<'a> {
    pub lower: Option<(&'a Value, bool)>,
    pub upper: Option<(&'a Value, bool)>,
}

pub(super) async fn scan_index_range_columns(
    view: &dyn KvView,
    table: &Table,
    index: &Index,
    equality_prefix: &[Value],
    range: Option<Range<'_>>,
    columns: &[Column],
) -> Result<Vec<Row>> {
    collect(scan_index_range(view, table, index, equality_prefix, range, columns).await?).await
}

async fn collect(mut iterator: Box<dyn RowIterator + '_>) -> Result<Vec<Row>> {
    let mut rows = Vec::new();
    while let Some(row) = iterator.next().await? {
        rows.push(row);
    }
    Ok(rows)
}

pub(super) async fn scan_index_range<'a>(
    view: &'a dyn KvView,
    table: &Table,
    index: &Index,
    equality_prefix: &[Value],
    range: Option<Range<'_>>,
    columns: &[Column],
) -> Result<Box<dyn RowIterator + 'a>> {
    scan_index_range_with_pruning(view, table, index, equality_prefix, range, columns, None).await
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn scan_index_range_with_pruning<'a>(
    view: &'a dyn KvView,
    table: &Table,
    index: &Index,
    equality_prefix: &[Value],
    range: Option<Range<'_>>,
    columns: &[Column],
    pruning: Option<&ScanPruning>,
) -> Result<Box<dyn RowIterator + 'a>> {
    let mut prefix = codec::index_prefix(table, &index.id)?;
    prefix.extend_from_slice(&codec::encode_tuple(equality_prefix)?);
    let mut start = prefix.clone();
    let mut end = prefix_end(&prefix);
    if let Some(range) = range {
        if let Some((lower, inclusive)) = range.lower {
            let mut bound = prefix.clone();
            bound.extend_from_slice(&codec::encode_value(lower)?);
            start = if inclusive {
                bound
            } else {
                let Some(end) = prefix_end(&bound) else {
                    return Ok(Box::new(EmptyIterator));
                };
                end
            };
        }
        if let Some((upper, inclusive)) = range.upper {
            let mut bound = prefix.clone();
            bound.extend_from_slice(&codec::encode_value(upper)?);
            end = if inclusive {
                prefix_end(&bound)
            } else {
                Some(bound)
            };
        }
    }
    let range = KeyRange {
        start: Some(Bytes::from(start)),
        end: end.map(Bytes::from),
    };
    let Some(iterator) = open_scan(view, range, pruning).await? else {
        return Ok(Box::new(EmptyIterator));
    };
    Ok(Box::new(IndexIterator {
        view,
        iterator,
        table: table.clone(),
        index: index.clone(),
        columns: columns.to_vec(),
    }))
}

struct EmptyIterator;

async fn open_scan<'a>(
    view: &'a dyn KvView,
    range: KeyRange,
    pruning: Option<&ScanPruning>,
) -> Result<Option<Box<dyn KvIterator + 'a>>> {
    let Some(pruning) = pruning else {
        let Some(range) = nonempty_range(range) else {
            return Ok(None);
        };
        return Ok(Some(view.scan(range).await?));
    };
    let ScanPruning::Range(_) = pruning else {
        return Ok(None);
    };
    let Some(pruned) = pruned_range(range.clone(), Some(pruning)) else {
        return Ok(None);
    };
    let forward_seek = stricter_start(&range.start, &pruned.start);
    let initial = KeyRange {
        start: range.start,
        end: pruned.end,
    };
    let mut iterator = view
        .scan_with_request(ScanRequest::cascade_range(initial))
        .await?
        .iterator;
    if let Some(next_key) = forward_seek {
        iterator.seek_forward(&next_key).await?;
    }
    Ok(Some(iterator))
}

fn stricter_start(original: &Option<Bytes>, pruned: &Option<Bytes>) -> Option<Bytes> {
    pruned
        .as_ref()
        .filter(|pruned| original.as_ref().is_none_or(|original| *pruned > original))
        .cloned()
}

fn pruned_range(range: KeyRange, pruning: Option<&ScanPruning>) -> Option<KeyRange> {
    let Some(pruning) = pruning else {
        return nonempty_range(range);
    };
    let ScanPruning::Range(pruning) = pruning else {
        return None;
    };
    nonempty_range(KeyRange {
        start: maximum_start(range.start, pruning.start.clone()),
        end: minimum_end(range.end, pruning.end.clone()),
    })
}

fn maximum_start(first: Option<Bytes>, second: Option<Bytes>) -> Option<Bytes> {
    match (first, second) {
        (Some(first), Some(second)) => Some(first.max(second)),
        (first, second) => first.or(second),
    }
}

fn minimum_end(first: Option<Bytes>, second: Option<Bytes>) -> Option<Bytes> {
    match (first, second) {
        (Some(first), Some(second)) => Some(first.min(second)),
        (Some(end), None) | (None, Some(end)) => Some(end),
        (None, None) => None,
    }
}

fn nonempty_range(range: KeyRange) -> Option<KeyRange> {
    if range
        .start
        .as_ref()
        .zip(range.end.as_ref())
        .is_some_and(|(start, end)| start >= end)
    {
        None
    } else {
        Some(range)
    }
}

#[async_trait]
impl RowIterator for EmptyIterator {
    async fn next(&mut self) -> Result<Option<Row>> {
        Ok(None)
    }
}

struct IndexIterator<'a> {
    view: &'a dyn KvView,
    iterator: Box<dyn KvIterator + 'a>,
    table: Table,
    index: Index,
    columns: Vec<Column>,
}

#[async_trait]
impl RowIterator for IndexIterator<'_> {
    async fn next(&mut self) -> Result<Option<Row>> {
        let Some(entry) = self.iterator.next().await? else {
            return Ok(None);
        };
        let key = codec::data_key(&self.table, &entry.value)?;
        let raw = self.view.get(&key).await?.ok_or_else(|| {
            Error::message(
                ErrorKind::CorruptData,
                format!(
                    "exec: index {:?} points at a missing row of {:?}",
                    self.index.name, self.table.name
                ),
            )
        })?;
        Ok(Some(codec::unmarshal_row_columns(
            &self.table,
            &self.columns,
            &raw,
        )?))
    }
}
