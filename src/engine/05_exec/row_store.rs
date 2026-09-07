//! Physical row access over the ordered KV layout.

use async_trait::async_trait;
use bytes::Bytes;
use futures::FutureExt as _;
use futures::future::{self, BoxFuture};
use futures::stream::{FuturesOrdered, StreamExt};
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
#[cfg(debug_assertions)]
use std::time::Instant;
#[cfg(debug_assertions)]
use tracing::Instrument as _;

use crate::engine::catalog::model::{Column, Index, Table};
use crate::engine::kv::key_encoding::prefix_end;
use crate::engine::kv::{KeyRange, KvIterator, KvView, ScanOrder, ScanProfile, ScanRequest};
use crate::engine::lir::{Row, Value};

use super::codec;
use super::{Error, ErrorKind, Result};

const INDEX_BASE_ROW_READ_AHEAD: usize = 8;
#[cfg(debug_assertions)]
const DEBUG_TABLE_SCAN_SAMPLE_ROWS: u64 = 2_048;

#[derive(Clone, Default)]
pub(super) struct IndexReadTally {
    entries: Arc<AtomicU64>,
    reads: Arc<AtomicU64>,
    active: Arc<AtomicU64>,
    peak_active: Arc<AtomicU64>,
}

impl IndexReadTally {
    fn visit_entry(&self) {
        self.entries.fetch_add(1, Ordering::Relaxed);
    }

    fn start(&self) -> ActiveIndexRead<'_> {
        self.reads.fetch_add(1, Ordering::Relaxed);
        let active = self.active.fetch_add(1, Ordering::Relaxed) + 1;
        self.peak_active.fetch_max(active, Ordering::Relaxed);
        ActiveIndexRead(self)
    }

    pub(super) fn reads(&self) -> u64 {
        self.reads.load(Ordering::Relaxed)
    }

    pub(super) fn entries(&self) -> u64 {
        self.entries.load(Ordering::Relaxed)
    }

    pub(super) fn peak_active(&self) -> u64 {
        self.peak_active.load(Ordering::Relaxed)
    }
}

struct ActiveIndexRead<'a>(&'a IndexReadTally);

impl Drop for ActiveIndexRead<'_> {
    fn drop(&mut self) {
        self.0.active.fetch_sub(1, Ordering::Relaxed);
    }
}

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
    collect(scan_table(view, table, columns).await?, columns).await
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
    async fn next(&mut self) -> Result<Option<codec::DecodedRow>>;

    async fn next_batch(
        &mut self,
        limit: usize,
        output: &mut Vec<codec::DecodedRow>,
    ) -> Result<()> {
        let target = output.len().saturating_add(limit);
        while output.len() < target {
            let Some(row) = self.next().await? else {
                break;
            };
            output.push(row);
        }
        Ok(())
    }

    fn enable_bounded_read_ahead(&mut self) {}

    fn raw_decoder(&self) -> Option<codec::RowDecoder> {
        None
    }

    async fn next_raw_batch(&mut self, _limit: usize, _output: &mut Vec<Bytes>) -> Result<()> {
        Err(Error::message(
            ErrorKind::Internal,
            "exec: row iterator does not provide raw rows",
        ))
    }
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
    let Some(iterator) = open_scan(
        view,
        range,
        pruning,
        ScanOrder::Ascending,
        ScanProfile::Throughput,
    )
    .await?
    else {
        return Ok(Box::new(EmptyIterator));
    };
    Ok(Box::new(TableIterator {
        iterator,
        #[cfg(not(debug_assertions))]
        entry_batch: Vec::new(),
        prefix,
        table: table.clone(),
        decoder: codec::RowDecoder::new(table, columns)?,
        #[cfg(debug_assertions)]
        debug: DebugTableScan::new(table, columns.len()),
    }))
}

struct TableIterator<'a> {
    iterator: Box<dyn KvIterator + 'a>,
    #[cfg(not(debug_assertions))]
    entry_batch: Vec<crate::engine::kv::Entry>,
    prefix: Vec<u8>,
    table: Table,
    decoder: codec::RowDecoder,
    #[cfg(debug_assertions)]
    debug: DebugTableScan,
}

impl TableIterator<'_> {
    async fn read_next(&mut self) -> Result<Option<codec::DecodedRow>> {
        #[cfg(debug_assertions)]
        self.debug.start();
        #[cfg(debug_assertions)]
        let entry = {
            let started = Instant::now();
            let sample = self.debug.storage_sample();
            let result = if let Some(span) = sample.as_ref() {
                self.iterator.next().instrument(span.clone()).await
            } else {
                self.iterator.next().await
            };
            self.debug
                .record_storage(started.elapsed(), sample.as_ref(), &result);
            result
        };
        #[cfg(not(debug_assertions))]
        let entry = self.iterator.next().await;
        let Some(entry) = entry? else {
            #[cfg(debug_assertions)]
            self.debug.finish(true, None);
            return Ok(None);
        };
        if !entry.key.starts_with(&self.prefix) {
            #[cfg(debug_assertions)]
            self.debug.finish(false, Some("key_prefix"));
            return Err(Error::message(
                ErrorKind::CorruptData,
                format!(
                    "exec: table scan for {:?} escaped its prefix",
                    self.table.name
                ),
            ));
        }
        #[cfg(debug_assertions)]
        let decoded = {
            let started = Instant::now();
            let sample = self.debug.decode_sample(entry.value.len());
            let _entered = sample.as_ref().map(tracing::Span::enter);
            let result = self.decoder.decode(&entry.value);
            drop(_entered);
            self.debug.record_decode(
                started.elapsed(),
                sample.as_ref(),
                entry.value.len(),
                &result,
            );
            result
        };
        #[cfg(not(debug_assertions))]
        let decoded = self.decoder.decode(&entry.value);
        match decoded {
            Ok(row) => {
                #[cfg(debug_assertions)]
                self.debug.record_row(entry.value.len());
                Ok(Some(row))
            }
            Err(error) => {
                #[cfg(debug_assertions)]
                self.debug.finish(false, Some("row_decode"));
                Err(error)
            }
        }
    }
}

#[async_trait]
impl RowIterator for TableIterator<'_> {
    async fn next(&mut self) -> Result<Option<codec::DecodedRow>> {
        self.read_next().await
    }

    async fn next_batch(
        &mut self,
        limit: usize,
        output: &mut Vec<codec::DecodedRow>,
    ) -> Result<()> {
        #[cfg(not(debug_assertions))]
        {
            self.entry_batch.clear();
            self.iterator
                .next_batch(limit, &mut self.entry_batch)
                .await?;
            let prefix = &self.prefix;
            let table = &self.table;
            let decoder = &self.decoder;
            output.reserve(self.entry_batch.len());
            for entry in self.entry_batch.drain(..) {
                if !entry.key.starts_with(prefix) {
                    return Err(Error::message(
                        ErrorKind::CorruptData,
                        format!("exec: table scan for {:?} escaped its prefix", table.name),
                    ));
                }
                output.push(decoder.decode(&entry.value)?);
            }
            return Ok(());
        }
        #[cfg(debug_assertions)]
        let target = output.len().saturating_add(limit);
        #[cfg(debug_assertions)]
        while output.len() < target {
            let Some(row) = self.read_next().await? else {
                break;
            };
            output.push(row);
        }
        #[cfg(debug_assertions)]
        Ok(())
    }

    fn raw_decoder(&self) -> Option<codec::RowDecoder> {
        #[cfg(not(debug_assertions))]
        {
            Some(self.decoder.clone())
        }
        #[cfg(debug_assertions)]
        {
            None
        }
    }

    async fn next_raw_batch(&mut self, limit: usize, output: &mut Vec<Bytes>) -> Result<()> {
        #[cfg(not(debug_assertions))]
        {
            self.entry_batch.clear();
            self.iterator
                .next_batch(limit, &mut self.entry_batch)
                .await?;
            let prefix = &self.prefix;
            let table = &self.table;
            output.reserve(self.entry_batch.len());
            for entry in self.entry_batch.drain(..) {
                if !entry.key.starts_with(prefix) {
                    return Err(Error::message(
                        ErrorKind::CorruptData,
                        format!("exec: table scan for {:?} escaped its prefix", table.name),
                    ));
                }
                output.push(entry.value);
            }
            return Ok(());
        }
        #[cfg(debug_assertions)]
        {
            let _ = (limit, output);
            Err(Error::message(
                ErrorKind::Internal,
                "exec: debug row iterator does not provide raw rows",
            ))
        }
    }
}

#[cfg(debug_assertions)]
struct DebugTableScan {
    summary: Option<tracing::Span>,
    table_schema_id: u32,
    selected_columns: u64,
    rows: u64,
    bytes: u64,
    storage_nanos: u64,
    decode_nanos: u64,
}

#[cfg(debug_assertions)]
impl DebugTableScan {
    fn new(table: &Table, selected_columns: usize) -> Self {
        Self {
            summary: None,
            table_schema_id: table.schema_id.into(),
            selected_columns: selected_columns as u64,
            rows: 0,
            bytes: 0,
            storage_nanos: 0,
            decode_nanos: 0,
        }
    }

    fn start(&mut self) {
        if self.summary.is_some()
            || !tracing::enabled!(target: "rad::telemetry", tracing::Level::DEBUG)
        {
            return;
        }
        self.summary = Some(tracing::debug_span!(
            target: "rad::telemetry",
            "rad.debug.table_scan",
            otel.name = "rad.debug.table_scan",
            otel.kind = "internal",
            rad.debug.scan.table_schema_id = self.table_schema_id,
            rad.debug.scan.selected_columns = self.selected_columns,
            rad.debug.scan.sample_every_rows = DEBUG_TABLE_SCAN_SAMPLE_ROWS,
            rad.debug.scan.rows = tracing::field::Empty,
            rad.debug.scan.bytes = tracing::field::Empty,
            rad.debug.scan.storage_duration_us = tracing::field::Empty,
            rad.debug.scan.decode_duration_us = tracing::field::Empty,
            rad.debug.scan.complete = tracing::field::Empty,
            rad.debug.scan.failure_phase = tracing::field::Empty,
            rad.status = tracing::field::Empty,
            otel.status_code = tracing::field::Empty,
        ));
    }

    fn storage_sample(&self) -> Option<tracing::Span> {
        if !self.rows.is_multiple_of(DEBUG_TABLE_SCAN_SAMPLE_ROWS) {
            return None;
        }
        let parent = self.summary.as_ref()?;
        Some(tracing::debug_span!(
            target: "rad::telemetry",
            parent: parent,
            "rad.debug.table_scan.storage_sample",
            otel.name = "rad.debug.table_scan.storage_sample",
            otel.kind = "internal",
            rad.debug.scan.row_index = self.rows,
            rad.debug.scan.result = tracing::field::Empty,
            rad.debug.scan.bytes = tracing::field::Empty,
            rad.status = tracing::field::Empty,
            otel.status_code = tracing::field::Empty,
        ))
    }

    fn decode_sample(&self, bytes: usize) -> Option<tracing::Span> {
        if !self.rows.is_multiple_of(DEBUG_TABLE_SCAN_SAMPLE_ROWS) {
            return None;
        }
        let parent = self.summary.as_ref()?;
        Some(tracing::debug_span!(
            target: "rad::telemetry",
            parent: parent,
            "rad.debug.table_scan.decode_sample",
            otel.name = "rad.debug.table_scan.decode_sample",
            otel.kind = "internal",
            rad.debug.scan.row_index = self.rows,
            rad.debug.scan.bytes = bytes as u64,
            rad.status = tracing::field::Empty,
            otel.status_code = tracing::field::Empty,
        ))
    }

    fn record_storage(
        &mut self,
        duration: std::time::Duration,
        sample: Option<&tracing::Span>,
        result: &crate::engine::kv::Result<Option<crate::engine::kv::Entry>>,
    ) {
        self.storage_nanos = self
            .storage_nanos
            .saturating_add(duration.as_nanos().min(u128::from(u64::MAX)) as u64);
        let Some(sample) = sample else {
            return;
        };
        match result {
            Ok(Some(entry)) => {
                sample.record("rad.debug.scan.result", "row");
                sample.record("rad.debug.scan.bytes", entry.value.len() as u64);
                sample.record("rad.status", "success");
            }
            Ok(None) => {
                sample.record("rad.debug.scan.result", "end");
                sample.record("rad.status", "success");
            }
            Err(_) => {
                sample.record("rad.debug.scan.result", "error");
                sample.record("rad.status", "error");
                sample.record("otel.status_code", "ERROR");
            }
        }
    }

    fn record_decode<T>(
        &mut self,
        duration: std::time::Duration,
        sample: Option<&tracing::Span>,
        bytes: usize,
        result: &Result<T>,
    ) {
        self.decode_nanos = self
            .decode_nanos
            .saturating_add(duration.as_nanos().min(u128::from(u64::MAX)) as u64);
        let Some(sample) = sample else {
            return;
        };
        sample.record("rad.debug.scan.bytes", bytes as u64);
        sample.record(
            "rad.status",
            if result.is_ok() { "success" } else { "error" },
        );
        if result.is_err() {
            sample.record("otel.status_code", "ERROR");
        }
    }

    fn record_row(&mut self, bytes: usize) {
        self.rows = self.rows.saturating_add(1);
        self.bytes = self.bytes.saturating_add(bytes as u64);
    }

    fn finish(&mut self, complete: bool, failure_phase: Option<&'static str>) {
        let Some(span) = self.summary.take() else {
            return;
        };
        span.record("rad.debug.scan.rows", self.rows);
        span.record("rad.debug.scan.bytes", self.bytes);
        span.record(
            "rad.debug.scan.storage_duration_us",
            self.storage_nanos / 1_000,
        );
        span.record(
            "rad.debug.scan.decode_duration_us",
            self.decode_nanos / 1_000,
        );
        span.record("rad.debug.scan.complete", complete);
        if let Some(failure_phase) = failure_phase {
            span.record("rad.debug.scan.failure_phase", failure_phase);
            span.record("rad.status", "error");
            span.record("otel.status_code", "ERROR");
        } else {
            span.record("rad.status", "success");
        }
    }
}

#[cfg(debug_assertions)]
impl Drop for DebugTableScan {
    fn drop(&mut self) {
        self.finish(false, None);
    }
}

pub(super) struct Range<'a> {
    pub lower: Option<(&'a Value, bool)>,
    pub upper: Option<(&'a Value, bool)>,
}

async fn collect(mut iterator: Box<dyn RowIterator + '_>, columns: &[Column]) -> Result<Vec<Row>> {
    let mut rows = Vec::new();
    while let Some(values) = iterator.next().await? {
        rows.push(column_values_to_row(columns, values));
    }
    Ok(rows)
}

fn column_values_to_row(columns: &[Column], values: codec::DecodedRow) -> Row {
    debug_assert_eq!(columns.len(), values.len());
    columns
        .iter()
        .zip(values)
        .map(|(column, value)| (column.name.clone(), value))
        .collect()
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn scan_index_range<'a>(
    view: &'a dyn KvView,
    table: &Table,
    index: &Index,
    equality_prefix: &[Value],
    range: Option<Range<'_>>,
    descending_prefix: usize,
    descending_limit: Option<usize>,
    columns: &[Column],
) -> Result<Box<dyn RowIterator + 'a>> {
    scan_index_range_with_options(
        view,
        table,
        index,
        equality_prefix,
        range,
        descending_prefix,
        descending_limit,
        columns,
        None,
        None,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn scan_index_range_observed<'a>(
    view: &'a dyn KvView,
    table: &Table,
    index: &Index,
    equality_prefix: &[Value],
    range: Option<Range<'_>>,
    descending_prefix: usize,
    descending_limit: Option<usize>,
    columns: &[Column],
    tally: Option<IndexReadTally>,
) -> Result<Box<dyn RowIterator + 'a>> {
    scan_index_range_with_options(
        view,
        table,
        index,
        equality_prefix,
        range,
        descending_prefix,
        descending_limit,
        columns,
        None,
        tally,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn scan_index_range_with_pruning<'a>(
    view: &'a dyn KvView,
    table: &Table,
    index: &Index,
    equality_prefix: &[Value],
    range: Option<Range<'_>>,
    descending_prefix: usize,
    descending_limit: Option<usize>,
    columns: &[Column],
    pruning: Option<&ScanPruning>,
) -> Result<Box<dyn RowIterator + 'a>> {
    scan_index_range_with_options(
        view,
        table,
        index,
        equality_prefix,
        range,
        descending_prefix,
        descending_limit,
        columns,
        pruning,
        None,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn scan_index_range_with_options<'a>(
    view: &'a dyn KvView,
    table: &Table,
    index: &Index,
    equality_prefix: &[Value],
    range: Option<Range<'_>>,
    descending_prefix: usize,
    descending_limit: Option<usize>,
    columns: &[Column],
    pruning: Option<&ScanPruning>,
    tally: Option<IndexReadTally>,
) -> Result<Box<dyn RowIterator + 'a>> {
    let mut prefix = codec::index_prefix(table, &index.id)?;
    let tuple_offset = prefix.len();
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
    let native_descending =
        descending_prefix > 0 && descending_limit.is_some() && pruning.is_none();
    let order = if native_descending {
        ScanOrder::Descending
    } else {
        ScanOrder::Ascending
    };
    let Some(iterator) = open_scan(view, range, pruning, order, ScanProfile::Latency).await? else {
        return Ok(Box::new(EmptyIterator));
    };
    Ok(Box::new(IndexIterator {
        view,
        iterator,
        table: table.clone(),
        index: index.clone(),
        decoder: codec::RowDecoder::new(table, columns)?,
        pending: FuturesOrdered::new(),
        descending_primary_keys: None,
        descending_group_values: (descending_prefix > 0)
            .then_some(equality_prefix.len().saturating_add(descending_prefix)),
        descending_limit,
        native_descending,
        tuple_offset,
        exhausted: false,
        read_ahead: if descending_prefix > 0 {
            INDEX_BASE_ROW_READ_AHEAD
        } else {
            1
        },
        tally,
    }))
}

struct EmptyIterator;

async fn open_scan<'a>(
    view: &'a dyn KvView,
    range: KeyRange,
    pruning: Option<&ScanPruning>,
    order: ScanOrder,
    profile: ScanProfile,
) -> Result<Option<Box<dyn KvIterator + 'a>>> {
    let Some(pruning) = pruning else {
        let Some(range) = nonempty_range(range) else {
            return Ok(None);
        };
        return Ok(Some(
            view.scan_with_request(
                ScanRequest::access_path(range)
                    .with_order(order)
                    .with_profile(profile),
            )
            .await?
            .iterator,
        ));
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
        .scan_with_request(ScanRequest::cascade_range(initial).with_profile(profile))
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
    async fn next(&mut self) -> Result<Option<codec::DecodedRow>> {
        Ok(None)
    }
}

struct IndexIterator<'a> {
    view: &'a dyn KvView,
    iterator: Box<dyn KvIterator + 'a>,
    table: Table,
    index: Index,
    decoder: codec::RowDecoder,
    pending: FuturesOrdered<BoxFuture<'a, Result<Option<Bytes>>>>,
    descending_primary_keys: Option<VecDeque<Bytes>>,
    descending_group_values: Option<usize>,
    descending_limit: Option<usize>,
    native_descending: bool,
    tuple_offset: usize,
    exhausted: bool,
    read_ahead: usize,
    tally: Option<IndexReadTally>,
}

#[async_trait]
impl<'a> RowIterator for IndexIterator<'a> {
    async fn next(&mut self) -> Result<Option<codec::DecodedRow>> {
        if self.read_ahead == 1 {
            return self.next_sequential().await;
        }
        while !self.exhausted && self.pending.len() < self.read_ahead {
            let primary_key = match self.next_primary_key().await {
                Ok(Some(primary_key)) => primary_key,
                Ok(None) => {
                    self.exhausted = true;
                    break;
                }
                Err(error) => {
                    self.pending
                        .push_back(future::ready::<Result<Option<Bytes>>>(Err(error)).boxed());
                    self.exhausted = true;
                    break;
                }
            };
            let key = match codec::data_key(&self.table, &primary_key) {
                Ok(key) => key,
                Err(error) => {
                    self.pending.push_back(future::ready(Err(error)).boxed());
                    self.exhausted = true;
                    break;
                }
            };
            let view = self.view;
            let tally = self.tally.clone();
            self.pending.push_back(
                async move {
                    let _active = tally.as_ref().map(IndexReadTally::start);
                    Ok(view.get(&key).await?)
                }
                .boxed(),
            );
        }
        let Some(raw) = self.pending.next().await else {
            return Ok(None);
        };
        self.decode(raw?)
    }

    fn enable_bounded_read_ahead(&mut self) {
        self.read_ahead = INDEX_BASE_ROW_READ_AHEAD;
    }
}

impl IndexIterator<'_> {
    async fn next_sequential(&mut self) -> Result<Option<codec::DecodedRow>> {
        let Some(primary_key) = self.next_primary_key().await? else {
            return Ok(None);
        };
        let key = codec::data_key(&self.table, &primary_key)?;
        let _active = self.tally.as_ref().map(IndexReadTally::start);
        let raw = self.view.get(&key).await?;
        self.decode(raw)
    }

    async fn next_primary_key(&mut self) -> Result<Option<Bytes>> {
        if self.descending_group_values.is_some() {
            self.load_descending_primary_keys().await?;
            return Ok(self
                .descending_primary_keys
                .as_mut()
                .and_then(VecDeque::pop_front));
        }
        let Some(entry) = self.iterator.next().await? else {
            return Ok(None);
        };
        if let Some(tally) = &self.tally {
            tally.visit_entry();
        }
        Ok(Some(entry.value))
    }

    async fn load_descending_primary_keys(&mut self) -> Result<()> {
        if self.descending_primary_keys.is_some() {
            return Ok(());
        }
        if self.native_descending {
            return self.load_native_descending_primary_keys().await;
        }
        let group_values = self
            .descending_group_values
            .expect("descending index scan has an order prefix");
        let mut groups: VecDeque<(Vec<u8>, Vec<Bytes>)> = VecDeque::new();
        let mut retained = 0usize;
        while let Some(entry) = self.iterator.next().await? {
            if let Some(tally) = &self.tally {
                tally.visit_entry();
            }
            let group = index_order_group(&entry.key, self.tuple_offset, group_values)?;
            if let Some((current, primary_keys)) = groups.back_mut()
                && *current == group
            {
                primary_keys.push(entry.value);
            } else {
                groups.push_back((group, vec![entry.value]));
            }
            retained = retained.saturating_add(1);
            retain_descending_tail(&mut groups, &mut retained, self.descending_limit);
        }
        self.descending_primary_keys = Some(
            groups
                .into_iter()
                .rev()
                .flat_map(|(_, primary_keys)| primary_keys)
                .collect(),
        );
        Ok(())
    }

    async fn load_native_descending_primary_keys(&mut self) -> Result<()> {
        let group_values = self
            .descending_group_values
            .expect("descending index scan has an order prefix");
        let limit = self
            .descending_limit
            .expect("native descending index scan has a limit");
        let mut primary_keys = VecDeque::with_capacity(limit);
        let mut next_entry = self.next_index_entry().await?;
        while primary_keys.len() < limit {
            let Some(first) = next_entry.take() else {
                break;
            };
            let group = index_order_group(&first.key, self.tuple_offset, group_values)?;
            let remaining = limit.saturating_sub(primary_keys.len());
            let mut group_primary_keys = VecDeque::with_capacity(remaining);
            retain_lowest_primary_key(&mut group_primary_keys, first.value, remaining);
            loop {
                let Some(entry) = self.next_index_entry().await? else {
                    break;
                };
                let entry_group = index_order_group(&entry.key, self.tuple_offset, group_values)?;
                if entry_group != group {
                    next_entry = Some(entry);
                    break;
                }
                retain_lowest_primary_key(&mut group_primary_keys, entry.value, remaining);
            }
            while let Some(primary_key) = group_primary_keys.pop_back() {
                primary_keys.push_back(primary_key);
            }
        }
        self.descending_primary_keys = Some(primary_keys);
        Ok(())
    }

    async fn next_index_entry(&mut self) -> Result<Option<crate::engine::kv::Entry>> {
        let entry = self.iterator.next().await?;
        if entry.is_some()
            && let Some(tally) = &self.tally
        {
            tally.visit_entry();
        }
        Ok(entry)
    }

    fn decode(&self, raw: Option<Bytes>) -> Result<Option<codec::DecodedRow>> {
        let raw = raw.ok_or_else(|| {
            Error::message(
                ErrorKind::CorruptData,
                format!(
                    "exec: index {:?} points at a missing row of {:?}",
                    self.index.name, self.table.name
                ),
            )
        })?;
        Ok(Some(self.decoder.decode(&raw)?))
    }
}

fn retain_lowest_primary_key(primary_keys: &mut VecDeque<Bytes>, key: Bytes, limit: usize) {
    if limit == 0 {
        return;
    }
    primary_keys.push_back(key);
    if primary_keys.len() > limit {
        primary_keys.pop_front();
    }
}

fn retain_descending_tail(
    groups: &mut VecDeque<(Vec<u8>, Vec<Bytes>)>,
    retained: &mut usize,
    limit: Option<usize>,
) {
    let Some(limit) = limit else {
        return;
    };
    while *retained > limit {
        let excess = retained.saturating_sub(limit);
        let first_len = groups.front().map_or(0, |(_, rows)| rows.len());
        if first_len <= excess {
            groups.pop_front();
            *retained = retained.saturating_sub(first_len);
            continue;
        }
        let (_, rows) = groups.front_mut().expect("retained rows have a group");
        rows.truncate(first_len.saturating_sub(excess));
        *retained = retained.saturating_sub(excess);
    }
}

fn index_order_group(key: &[u8], tuple_offset: usize, values: usize) -> Result<Vec<u8>> {
    let tuple = key.get(tuple_offset..).ok_or_else(|| {
        Error::message(ErrorKind::CorruptData, "exec: truncated index key prefix")
    })?;
    let mut consumed = 0;
    for _ in 0..values {
        let (_, size) = codec::decode_value(&tuple[consumed..])?;
        consumed = consumed.saturating_add(size);
    }
    Ok(tuple[..consumed].to_vec())
}
