//! Table surveys: enumerate a table's population inside one snapshot and
//! produce the synopsis models the estimator consumes.
//!
//! A survey reads through a snapshot transaction that always rolls back. A
//! table within the row limit has complete coverage. A larger table reports
//! prefix coverage and an observed-row lower bound.

use std::collections::{HashMap, HashSet};

use sha2::{Digest as _, Sha256};

use crate::engine::catalog;
use crate::engine::catalog::model::Table;
use crate::engine::kv::{IsolationLevel, TransactionView};
use crate::engine::lir::Value;
use crate::engine::planner::models::{
    COLUMN_GROUP_MAX_COLUMNS, ColumnGroupSynopsis, ColumnSynopsis, MostCommonColumnGroup,
    MostCommonValue, SynopsisCoverage, SynopsisModel, SynopsisValue,
};

use super::{Engine, Result, row_store};

pub const SURVEY_ROW_CAP: u64 = 1_000_000;

pub struct SurveyRequest {
    pub changed: HashMap<catalog::identity::SchemaId, u64>,
    pub known: HashMap<catalog::identity::SchemaId, u64>,
    pub change_threshold: u64,
    pub max_age_micros: u64,
    pub now_micros: u64,
    pub row_budget: u64,
    pub byte_budget: u64,
}

pub struct SurveyResult {
    pub model: SynopsisModel,
    pub covered_changes: u64,
}

struct ColumnAccumulator {
    nulls: u64,
    distinct: DistinctAccumulator,
    most_common: MostCommonAccumulator,
    width: u64,
    minimum: Option<Value>,
    maximum: Option<Value>,
}

impl ColumnAccumulator {
    fn new() -> Self {
        Self {
            nulls: 0,
            distinct: DistinctAccumulator::new(),
            most_common: MostCommonAccumulator::new(),
            width: 0,
            minimum: None,
            maximum: None,
        }
    }

    fn observe(&mut self, value: &Value) {
        if value.is_null() {
            self.nulls += 1;
            return;
        }
        let (digest, width) = value_digest(value);
        self.width = self.width.saturating_add(width);
        self.distinct.observe(u64::from_be_bytes(
            digest[..8].try_into().expect("SHA-256 prefix"),
        ));
        self.most_common.observe(value, digest);
        let replace_minimum = match &self.minimum {
            Some(minimum) => matches!(value.compare(minimum), Ok(std::cmp::Ordering::Less)),
            None => true,
        };
        if replace_minimum {
            self.minimum = Some(value.clone());
        }
        let replace_maximum = match &self.maximum {
            Some(maximum) => matches!(value.compare(maximum), Ok(std::cmp::Ordering::Greater)),
            None => true,
        };
        if replace_maximum {
            self.maximum = Some(value.clone());
        }
    }

    fn synopsis(self, column: &catalog::model::Column, rows: u64) -> ColumnSynopsis {
        let observed = rows.saturating_sub(self.nulls);
        let (distinct, distinct_is_exact) = self.distinct.result();
        ColumnSynopsis {
            column: column.schema_id,
            value_generation: column.value_generation.get(),
            null_fraction: if rows == 0 {
                0.0
            } else {
                self.nulls as f64 / rows as f64
            },
            null_count: self.nulls,
            distinct,
            distinct_is_exact,
            average_width: self.width.checked_div(observed).unwrap_or(0),
            minimum: self.minimum.map(|value| value.to_string()),
            maximum: self.maximum.map(|value| value.to_string()),
            most_common_values: self.most_common.result(),
        }
    }
}

fn value_digest(value: &Value) -> ([u8; 32], u64) {
    let mut hasher = Sha256::new();
    let width = match value {
        Value::Text(text) => {
            hasher.update([0]);
            hasher.update(text.as_bytes());
            text.len() as u64
        }
        Value::Int64(value) => {
            hasher.update([1]);
            hasher.update(value.to_be_bytes());
            8
        }
        Value::Float64(value) => {
            hasher.update([2]);
            let bits = if *value == 0.0 { 0 } else { value.to_bits() };
            hasher.update(bits.to_be_bytes());
            8
        }
        Value::Bool(value) => {
            hasher.update([3, u8::from(*value)]);
            1
        }
        Value::Null(_) => unreachable!("nulls counted before hashing"),
    };
    (hasher.finalize().into(), width)
}

const MCV_CAPACITY: usize = 32;
const MCV_MAX_TEXT_BYTES: usize = 256;
const COLUMN_GROUP_CAPACITY: usize = 32;

struct MostCommonAccumulator {
    entries: Vec<MostCommonEntry>,
}

struct MostCommonEntry {
    digest: [u8; 32],
    value: SynopsisValue,
    frequency: u64,
    maximum_error: u64,
}

impl MostCommonAccumulator {
    fn new() -> Self {
        Self {
            entries: Vec::with_capacity(MCV_CAPACITY),
        }
    }

    fn observe(&mut self, value: &Value, digest: [u8; 32]) {
        if matches!(value, Value::Text(text) if text.len() > MCV_MAX_TEXT_BYTES) {
            return;
        }
        if let Some(entry) = self
            .entries
            .iter_mut()
            .find(|entry| entry.digest == digest && entry.value.storage_eq(value))
        {
            entry.frequency = entry.frequency.saturating_add(1);
            return;
        }
        let value = SynopsisValue::of(value).expect("nulls do not enter MCV collection");
        if self.entries.len() < MCV_CAPACITY {
            self.entries.push(MostCommonEntry {
                digest,
                value,
                frequency: 1,
                maximum_error: 0,
            });
            return;
        }
        let victim = self
            .entries
            .iter()
            .enumerate()
            .min_by_key(|(_, entry)| (entry.frequency, entry.digest))
            .map(|(index, _)| index)
            .expect("MCV capacity is positive");
        let frequency = self.entries[victim].frequency;
        self.entries[victim] = MostCommonEntry {
            digest,
            value,
            frequency: frequency.saturating_add(1),
            maximum_error: frequency,
        };
    }

    fn result(mut self) -> Vec<MostCommonValue> {
        self.entries
            .sort_by_key(|entry| (std::cmp::Reverse(entry.frequency), entry.digest));
        self.entries
            .into_iter()
            .map(|entry| MostCommonValue {
                value: entry.value,
                frequency: entry.frequency,
                maximum_error: entry.maximum_error,
            })
            .collect()
    }
}

struct ColumnGroupAccumulator {
    column_indexes: Vec<usize>,
    nulls: u64,
    distinct: DistinctAccumulator,
    most_common: MostCommonColumnGroupAccumulator,
}

impl ColumnGroupAccumulator {
    fn new(column_indexes: Vec<usize>) -> Self {
        Self {
            column_indexes,
            nulls: 0,
            distinct: DistinctAccumulator::new(),
            most_common: MostCommonColumnGroupAccumulator::new(),
        }
    }

    fn observe(&mut self, values: &[Value]) {
        if values.iter().any(Value::is_null) {
            self.nulls = self.nulls.saturating_add(1);
            return;
        }
        let mut hasher = Sha256::new();
        for value in values {
            hasher.update(value_digest(value).0);
        }
        let digest: [u8; 32] = hasher.finalize().into();
        self.distinct.observe(u64::from_be_bytes(
            digest[..8].try_into().expect("SHA-256 prefix"),
        ));
        self.most_common.observe(values, digest);
    }

    fn synopsis(self, table: &Table) -> ColumnGroupSynopsis {
        let (distinct, distinct_is_exact) = self.distinct.result();
        ColumnGroupSynopsis {
            columns: self
                .column_indexes
                .iter()
                .map(|index| table.columns[*index].schema_id)
                .collect(),
            value_generations: self
                .column_indexes
                .iter()
                .map(|index| table.columns[*index].value_generation.get())
                .collect(),
            null_count: self.nulls,
            distinct,
            distinct_is_exact,
            most_common_values: self.most_common.result(),
        }
    }
}

struct MostCommonColumnGroupAccumulator {
    entries: Vec<MostCommonColumnGroupEntry>,
}

struct MostCommonColumnGroupEntry {
    digest: [u8; 32],
    values: Vec<SynopsisValue>,
    frequency: u64,
    maximum_error: u64,
}

impl MostCommonColumnGroupAccumulator {
    fn new() -> Self {
        Self {
            entries: Vec::with_capacity(MCV_CAPACITY),
        }
    }

    fn observe(&mut self, values: &[Value], digest: [u8; 32]) {
        let text_bytes = values
            .iter()
            .filter_map(|value| match value {
                Value::Text(text) => Some(text.len()),
                _ => None,
            })
            .fold(0usize, usize::saturating_add);
        if text_bytes > MCV_MAX_TEXT_BYTES {
            return;
        }
        if let Some(entry) = self.entries.iter_mut().find(|entry| {
            entry.digest == digest
                && entry.values.len() == values.len()
                && entry
                    .values
                    .iter()
                    .zip(values)
                    .all(|(left, right)| left.storage_eq(right))
        }) {
            entry.frequency = entry.frequency.saturating_add(1);
            return;
        }
        let values = values
            .iter()
            .map(|value| SynopsisValue::of(value).expect("nulls do not enter group collection"))
            .collect();
        if self.entries.len() < MCV_CAPACITY {
            self.entries.push(MostCommonColumnGroupEntry {
                digest,
                values,
                frequency: 1,
                maximum_error: 0,
            });
            return;
        }
        let victim = self
            .entries
            .iter()
            .enumerate()
            .min_by_key(|(_, entry)| (entry.frequency, entry.digest))
            .map(|(index, _)| index)
            .expect("MCV capacity is positive");
        let frequency = self.entries[victim].frequency;
        self.entries[victim] = MostCommonColumnGroupEntry {
            digest,
            values,
            frequency: frequency.saturating_add(1),
            maximum_error: frequency,
        };
    }

    fn result(mut self) -> Vec<MostCommonColumnGroup> {
        self.entries
            .sort_by_key(|entry| (std::cmp::Reverse(entry.frequency), entry.digest));
        self.entries
            .into_iter()
            .map(|entry| MostCommonColumnGroup {
                values: entry.values,
                frequency: entry.frequency,
                maximum_error: entry.maximum_error,
            })
            .collect()
    }
}

fn column_group_accumulators(table: &Table) -> Vec<ColumnGroupAccumulator> {
    let mut groups: HashMap<Vec<usize>, u8> = HashMap::new();
    let mut add_prefixes = |names: Vec<&str>, priority: u8| {
        let Some(mut column_indexes) = names
            .into_iter()
            .map(|name| table.columns.iter().position(|column| column.name == name))
            .collect::<Option<Vec<_>>>()
        else {
            return;
        };
        column_indexes.truncate(COLUMN_GROUP_MAX_COLUMNS);
        for length in 2..=column_indexes.len() {
            let mut group = column_indexes[..length].to_vec();
            group.sort_by_key(|position| table.columns[*position].schema_id);
            groups
                .entry(group)
                .and_modify(|current| *current = (*current).min(priority))
                .or_insert(priority);
        }
    };
    add_prefixes(table.primary_key.iter().map(String::as_str).collect(), 0);
    for index in table.indexes.iter().filter(|index| index.is_ready()) {
        add_prefixes(table.index_column_names(index), 2);
    }
    for foreign_key in &table.foreign_keys {
        let Some(mut group) = foreign_key
            .columns
            .iter()
            .map(|name| table.columns.iter().position(|column| column.name == *name))
            .collect::<Option<Vec<_>>>()
        else {
            continue;
        };
        if !(2..=COLUMN_GROUP_MAX_COLUMNS).contains(&group.len()) {
            continue;
        }
        group.sort_by_key(|position| table.columns[*position].schema_id);
        groups
            .entry(group)
            .and_modify(|current| *current = (*current).min(1))
            .or_insert(1);
    }
    let mut groups: Vec<_> = groups.into_iter().collect();
    groups.sort_by_key(|(group, priority)| {
        (
            *priority,
            group
                .iter()
                .map(|position| table.columns[*position].schema_id)
                .collect::<Vec<_>>(),
        )
    });
    groups.truncate(COLUMN_GROUP_CAPACITY);
    groups
        .into_iter()
        .map(|(group, _)| ColumnGroupAccumulator::new(group))
        .collect()
}

const EXACT_DISTINCT_LIMIT: usize = 4096;
const HLL_REGISTER_COUNT: usize = 256;

struct DistinctAccumulator {
    exact: Option<HashSet<u64>>,
    registers: [u8; HLL_REGISTER_COUNT],
}

impl DistinctAccumulator {
    fn new() -> Self {
        Self {
            exact: Some(HashSet::new()),
            registers: [0; HLL_REGISTER_COUNT],
        }
    }

    fn observe(&mut self, hash: u64) {
        if let Some(exact) = &mut self.exact {
            exact.insert(hash);
            if exact.len() > EXACT_DISTINCT_LIMIT {
                self.exact = None;
            }
        }

        let index = hash as usize & (HLL_REGISTER_COUNT - 1);
        let remainder = hash >> HLL_REGISTER_COUNT.ilog2();
        let rank = remainder
            .leading_zeros()
            .saturating_sub(HLL_REGISTER_COUNT.ilog2())
            .saturating_add(1) as u8;
        self.registers[index] = self.registers[index].max(rank);
    }

    fn result(self) -> (u64, bool) {
        if let Some(exact) = self.exact {
            return (exact.len() as u64, true);
        }

        let register_count = HLL_REGISTER_COUNT as f64;
        let harmonic_sum: f64 = self
            .registers
            .iter()
            .map(|rank| 2f64.powi(-i32::from(*rank)))
            .sum();
        let raw = 0.7213 / (1.0 + 1.079 / register_count) * register_count.powi(2) / harmonic_sum;
        let empty = self
            .registers
            .iter()
            .filter(|register| **register == 0)
            .count();
        let estimate = if raw <= 2.5 * register_count && empty > 0 {
            register_count * (register_count / empty as f64).ln()
        } else {
            raw
        };
        (estimate.round() as u64, false)
    }
}

impl Engine {
    /// Survey every live table under one snapshot. Read-only: the
    /// transaction always rolls back.
    pub async fn survey(&self) -> Result<Vec<SynopsisModel>> {
        let transaction = self.store.begin(IsolationLevel::Snapshot).await?;
        let mut view = TransactionView(&*transaction);
        let revision = catalog::store::current_revision(&mut view).await?;
        let tables = catalog::store::list_tables(&mut view).await?;
        let mut models = Vec::with_capacity(tables.len());
        let collected_at = self.now_unix_micros();
        for table in &tables {
            models.push(survey_table(&view, table, revision.version.get(), collected_at).await?);
        }
        transaction.rollback();
        Ok(models)
    }

    pub async fn survey_next(&self, request: SurveyRequest) -> Result<Option<SurveyResult>> {
        let transaction = self.store.begin(IsolationLevel::Snapshot).await?;
        let result = async {
            let mut view = TransactionView(&*transaction);
            let revision = catalog::store::current_revision(&mut view).await?;
            let tables = catalog::store::list_tables(&mut view).await?;
            let Some(table) = select_table(&tables, &request) else {
                return Ok(None);
            };
            let covered_changes = request.changed.get(&table.schema_id).copied().unwrap_or(0);
            let model = survey_table_with_budget(
                &view,
                table,
                revision.version.get(),
                request.now_micros,
                request.row_budget,
                request.byte_budget,
            )
            .await?;
            Ok(Some(SurveyResult {
                model,
                covered_changes,
            }))
        }
        .await;
        transaction.rollback();
        result
    }
}

fn select_table<'a>(tables: &'a [Table], request: &SurveyRequest) -> Option<&'a Table> {
    let mut ordered: Vec<&Table> = tables.iter().collect();
    ordered.sort_by_key(|table| table.schema_id);

    ordered
        .iter()
        .copied()
        .find(|table| !request.known.contains_key(&table.schema_id))
        .or_else(|| {
            ordered
                .iter()
                .copied()
                .filter_map(|table| {
                    let changes = request.changed.get(&table.schema_id).copied().unwrap_or(0);
                    (changes >= request.change_threshold).then_some((changes, table))
                })
                .max_by_key(|(changes, table)| (*changes, std::cmp::Reverse(table.schema_id)))
                .map(|(_, table)| table)
        })
        .or_else(|| {
            let cutoff = request.now_micros.saturating_sub(request.max_age_micros);
            ordered
                .into_iter()
                .filter_map(|table| {
                    request
                        .known
                        .get(&table.schema_id)
                        .copied()
                        .filter(|collected_at| *collected_at <= cutoff)
                        .map(|collected_at| (collected_at, table))
                })
                .min_by_key(|(collected_at, table)| (*collected_at, table.schema_id))
                .map(|(_, table)| table)
        })
}

pub(super) async fn survey_table(
    view: &TransactionView<'_>,
    table: &Table,
    catalog_version: u64,
    collected_at_unix_micros: u64,
) -> Result<SynopsisModel> {
    survey_table_with_limit(
        view,
        table,
        catalog_version,
        collected_at_unix_micros,
        SURVEY_ROW_CAP,
    )
    .await
}

async fn survey_table_with_limit(
    view: &TransactionView<'_>,
    table: &Table,
    catalog_version: u64,
    collected_at_unix_micros: u64,
    row_limit: u64,
) -> Result<SynopsisModel> {
    survey_table_with_budget(
        view,
        table,
        catalog_version,
        collected_at_unix_micros,
        row_limit,
        u64::MAX,
    )
    .await
}

async fn survey_table_with_budget(
    view: &TransactionView<'_>,
    table: &Table,
    catalog_version: u64,
    collected_at_unix_micros: u64,
    row_limit: u64,
    byte_limit: u64,
) -> Result<SynopsisModel> {
    let mut iterator = row_store::scan_table(view, table, &table.columns).await?;
    let mut accumulators: Vec<ColumnAccumulator> = table
        .columns
        .iter()
        .map(|_| ColumnAccumulator::new())
        .collect();
    let mut group_accumulators = column_group_accumulators(table);
    let mut rows = 0u64;
    let mut bytes = 0u64;
    let mut exact = true;
    while let Some(row) = iterator.next().await? {
        if rows >= row_limit {
            exact = false;
            break;
        }
        let row_bytes = table
            .columns
            .iter()
            .filter_map(|column| row.get(&column.name))
            .map(value_width)
            .fold(0u64, u64::saturating_add);
        if rows > 0 && bytes.saturating_add(row_bytes) > byte_limit {
            exact = false;
            break;
        }
        rows += 1;
        bytes = bytes.saturating_add(row_bytes);
        for (column, accumulator) in table.columns.iter().zip(&mut accumulators) {
            let value = row
                .get(&column.name)
                .cloned()
                .unwrap_or(Value::Null(column.scalar_type));
            accumulator.observe(&value);
        }
        for accumulator in &mut group_accumulators {
            let values: Vec<_> = accumulator
                .column_indexes
                .iter()
                .map(|index| {
                    let column = &table.columns[*index];
                    row.get(&column.name)
                        .cloned()
                        .unwrap_or(Value::Null(column.scalar_type))
                })
                .collect();
            accumulator.observe(&values);
        }
    }
    Ok(SynopsisModel {
        table: table.schema_id,
        observed_rows: rows,
        coverage: if exact {
            SynopsisCoverage::Complete
        } else {
            SynopsisCoverage::PrefixLimit
        },
        sample_size: rows,
        changes_since_collection: 0,
        table_existence_generation: table.existence_generation.get(),
        collected_at_unix_micros,
        catalog_version,
        columns: table
            .columns
            .iter()
            .zip(accumulators)
            .map(|(column, accumulator)| accumulator.synopsis(column, rows))
            .collect(),
        column_groups: group_accumulators
            .into_iter()
            .map(|accumulator| accumulator.synopsis(table))
            .collect(),
    })
}

fn value_width(value: &Value) -> u64 {
    match value {
        Value::Text(value) => value.len() as u64,
        Value::Int64(_) | Value::Float64(_) => 8,
        Value::Bool(_) => 1,
        Value::Null(_) => 0,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;

    use crate::engine::catalog::identity::SchemaId;
    use crate::engine::catalog::model::{ColumnDraft, IndexDef, ScalarType, TableDraft};
    use crate::engine::exec::{CatalogPolicy, Program, Statement};
    use crate::engine::kv::slatedb::Store;
    use crate::engine::lir::{Kind, RawScalar, Relation, RootCardinality, RowsColumn};

    use super::*;

    #[tokio::test]
    async fn survey_produces_exact_synopses() {
        let store = Arc::new(Store::memory("survey-exact").await.unwrap());
        let engine = Engine::new(store.clone());
        let catalog = catalog::Catalog::new(store.clone());
        catalog
            .create_table(TableDraft {
                id: Some(SchemaId::new(1).unwrap()),
                name: "items".into(),
                columns: vec![
                    ColumnDraft {
                        id: Some(SchemaId::new(1).unwrap()),
                        name: "id".into(),
                        scalar_type: ScalarType::Text,
                        nullable: false,
                        format: String::new(),
                        default: None,
                    },
                    ColumnDraft {
                        id: Some(SchemaId::new(2).unwrap()),
                        name: "label".into(),
                        scalar_type: ScalarType::Text,
                        nullable: true,
                        format: String::new(),
                        default: None,
                    },
                ],
                primary_key: vec!["id".into()],
                indexes: vec![IndexDef {
                    name: "items_id_label_idx".into(),
                    columns: vec!["id".into(), "label".into()],
                    unique: false,
                }],
                foreign_keys: Vec::new(),
            })
            .await
            .unwrap();

        let rows: Vec<Vec<RawScalar>> = vec![
            vec![RawScalar::Text("a".into()), RawScalar::Text("x".into())],
            vec![RawScalar::Text("b".into()), RawScalar::Text("x".into())],
            vec![RawScalar::Text("c".into()), RawScalar::Null],
            vec![RawScalar::Text("d".into()), RawScalar::Text("y".into())],
        ];
        engine
            .execute_program(
                Program {
                    statements: vec![Statement::Create {
                        name: "seed".into(),
                        relation: crate::engine::lir::Query {
                            root: Relation::Rows {
                                scope: "input".into(),
                                columns: vec![
                                    RowsColumn {
                                        name: "id".into(),
                                        kind: Kind::Text,
                                        nullable: false,
                                    },
                                    RowsColumn {
                                        name: "label".into(),
                                        kind: Kind::Text,
                                        nullable: true,
                                    },
                                ],
                                values: rows,
                            },
                            cardinality: RootCardinality::Many,
                            bindings: HashMap::new(),
                        },
                        table: "items".into(),
                    }],
                    result: None,
                },
                CatalogPolicy::Forbidden,
            )
            .await
            .unwrap();

        let models = engine.survey().await.unwrap();
        assert_eq!(models.len(), 1);
        let model = &models[0];
        assert_eq!(model.observed_rows, 4);
        assert_eq!(model.coverage, SynopsisCoverage::Complete);
        assert_eq!(model.sample_size, 4);
        assert_eq!(model.columns.len(), 2);

        let id = &model.columns[0];
        assert_eq!(id.null_fraction, 0.0);
        assert_eq!(id.distinct, 4);
        assert!(id.distinct_is_exact);
        assert_eq!(id.minimum.as_deref(), Some("\"a\""));
        assert_eq!(id.maximum.as_deref(), Some("\"d\""));

        let label = &model.columns[1];
        assert_eq!(label.null_fraction, 0.25);
        assert_eq!(label.null_count, 1);
        assert_eq!(label.distinct, 2);
        assert_eq!(label.average_width, 1);
        assert_eq!(label.most_common_values.len(), 2);
        assert_eq!(
            label.most_common_values[0],
            MostCommonValue {
                value: SynopsisValue::Text("x".into()),
                frequency: 2,
                maximum_error: 0,
            }
        );
        assert_eq!(model.column_groups.len(), 1);
        let group = &model.column_groups[0];
        assert_eq!(group.columns.len(), 2);
        assert_eq!(group.value_generations.len(), 2);
        assert_eq!(group.null_count, 1);
        assert_eq!(group.distinct, 3);
        assert!(group.distinct_is_exact);
        assert_eq!(group.most_common_values.len(), 3);
        assert!(group.most_common_values.iter().any(|common| {
            common.frequency == 1
                && common.maximum_error == 0
                && common.values
                    == [
                        SynopsisValue::Text("a".into()),
                        SynopsisValue::Text("x".into()),
                    ]
        }));

        let transaction = engine.store.begin(IsolationLevel::Snapshot).await.unwrap();
        let mut view = TransactionView(&*transaction);
        let table = catalog::store::list_tables(&mut view)
            .await
            .unwrap()
            .remove(0);
        assert_eq!(
            model.columns[1].value_generation,
            table.columns[1].value_generation.get()
        );
        let capped = survey_table_with_limit(&view, &table, 1, 0, 2)
            .await
            .unwrap();
        let byte_capped = survey_table_with_budget(&view, &table, 1, 0, 10, 1)
            .await
            .unwrap();
        transaction.rollback();
        assert_eq!(capped.observed_rows, 2);
        assert_eq!(capped.coverage, SynopsisCoverage::PrefixLimit);
        assert_eq!(byte_capped.observed_rows, 1);
        assert_eq!(byte_capped.coverage, SynopsisCoverage::PrefixLimit);

        let request = |changed, known| SurveyRequest {
            changed,
            known,
            change_threshold: 10,
            max_age_micros: 50,
            now_micros: 120,
            row_budget: 10,
            byte_budget: 1024,
        };
        let first = engine
            .survey_next(request(HashMap::new(), HashMap::new()))
            .await
            .unwrap()
            .expect("missing synopsis");
        assert_eq!(first.model.table, table.schema_id);

        let known = HashMap::from([(table.schema_id, 100)]);
        assert!(
            engine
                .survey_next(request(HashMap::new(), known.clone()))
                .await
                .unwrap()
                .is_none()
        );
        let changed = HashMap::from([(table.schema_id, 10)]);
        let changed = engine
            .survey_next(request(changed, known))
            .await
            .unwrap()
            .expect("changed table");
        assert_eq!(changed.covered_changes, 10);
    }

    #[test]
    fn most_common_value_collection_is_bounded_and_tracks_skew() {
        let mut accumulator = MostCommonAccumulator::new();
        for index in 0..100 {
            let value = Value::Text(format!("value-{index}"));
            accumulator.observe(&value, value_digest(&value).0);
        }
        let hot = Value::Text("hot".into());
        for _ in 0..200 {
            accumulator.observe(&hot, value_digest(&hot).0);
        }

        let values = accumulator.result();
        assert_eq!(values.len(), MCV_CAPACITY);
        let hot = values
            .iter()
            .find(|entry| entry.value.storage_eq(&hot))
            .expect("hot value");
        assert_eq!(hot.lower_frequency(), 200);
        assert!(hot.frequency >= hot.lower_frequency());

        let mut oversized = MostCommonAccumulator::new();
        let value = Value::Text("x".repeat(MCV_MAX_TEXT_BYTES + 1));
        oversized.observe(&value, value_digest(&value).0);
        assert!(oversized.result().is_empty());
    }

    #[test]
    fn most_common_column_group_collection_is_bounded_and_tracks_skew() {
        let digest = |values: &[Value]| -> [u8; 32] {
            let mut hasher = Sha256::new();
            for value in values {
                hasher.update(value_digest(value).0);
            }
            hasher.finalize().into()
        };
        let mut accumulator = MostCommonColumnGroupAccumulator::new();
        for index in 0..100 {
            let values = [
                Value::Text(format!("board-{index}")),
                Value::Text("open".into()),
            ];
            accumulator.observe(&values, digest(&values));
        }
        let hot = [Value::Text("board-hot".into()), Value::Text("open".into())];
        for _ in 0..200 {
            accumulator.observe(&hot, digest(&hot));
        }

        let values = accumulator.result();
        assert_eq!(values.len(), MCV_CAPACITY);
        let hot = values
            .iter()
            .find(|entry| {
                entry.values.len() == hot.len()
                    && entry
                        .values
                        .iter()
                        .zip(&hot)
                        .all(|(left, right)| left.storage_eq(right))
            })
            .expect("hot column group");
        assert_eq!(hot.lower_frequency(), 200);

        let mut oversized = MostCommonColumnGroupAccumulator::new();
        let values = [
            Value::Text("x".repeat(MCV_MAX_TEXT_BYTES)),
            Value::Text("y".into()),
        ];
        oversized.observe(&values, digest(&values));
        assert!(oversized.result().is_empty());
    }

    #[test]
    fn composite_primary_keys_select_column_groups_without_a_secondary_index() {
        let mut table = crate::engine::planner::test_support::table();
        table.primary_key = vec!["board_id".into(), "status".into()];
        table.indexes.clear();

        let groups = column_group_accumulators(&table);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].column_indexes, [1, 2]);
    }

    #[test]
    fn composite_foreign_keys_select_column_groups_without_a_secondary_index() {
        let mut table = crate::engine::planner::test_support::table();
        table.indexes.clear();
        table
            .foreign_keys
            .push(crate::engine::catalog::model::ForeignKey {
                id: "board-status-fk".into(),
                name: "board_status_fk".into(),
                columns: vec!["board_id".into(), "status".into()],
                ref_table_id: "board-status-table".into(),
                ref_columns: vec!["board_id".into(), "status".into()],
            });

        let groups = column_group_accumulators(&table);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].column_indexes, [1, 2]);
    }

    #[test]
    fn signed_zero_has_one_statistical_identity() {
        let minus = Value::Float64(-0.0);
        let plus = Value::Float64(0.0);
        let minus_digest = value_digest(&minus).0;
        let plus_digest = value_digest(&plus).0;
        assert_eq!(minus_digest, plus_digest);

        let mut most_common = MostCommonAccumulator::new();
        most_common.observe(&minus, minus_digest);
        most_common.observe(&plus, plus_digest);
        let values = most_common.result();
        assert_eq!(values.len(), 1);
        assert_eq!(values[0].frequency, 2);
    }

    #[test]
    fn distinct_accumulator_has_bounded_exact_storage() {
        let mut accumulator = DistinctAccumulator::new();
        for value in 0..EXACT_DISTINCT_LIMIT as u64 + 1000 {
            accumulator.observe(value.wrapping_mul(0x9e37_79b9_7f4a_7c15));
        }
        let (distinct, exact) = accumulator.result();
        assert!(!exact);
        assert!(distinct > EXACT_DISTINCT_LIMIT as u64);
    }
}
