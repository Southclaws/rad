// Package exec is the physical execution layer: it turns physical plans
// (04_planner) and write requests into KV operations — scans, gets, index
// lookups, joins, and constraint-checked writes.
//
// Every write runs inside a SerializableSnapshot transaction, so a row and
// its index entries commit atomically and constraint checks (duplicate PK,
// unique index, FK parent existence) are protected from concurrent writers
// by the KV's conflict detection. Multi-statement transactions are exposed
// via Engine.Txn.
package exec

import (
	"context"
	"errors"
	"time"

	kv "github.com/Southclaws/rad/rad/engine/01_kv"
	catalog "github.com/Southclaws/rad/rad/engine/02_catalog"
	"github.com/Southclaws/rad/rad/engine/02_catalog/change"
	"github.com/Southclaws/rad/rad/engine/02_catalog/model"
	"github.com/Southclaws/rad/rad/engine/02_catalog/store"
	lir "github.com/Southclaws/rad/rad/engine/03_lir"
	"github.com/Southclaws/rad/rad/engine/05_exec/mutate"
	"github.com/Southclaws/rad/rad/engine/05_exec/rowstore"
	"github.com/Southclaws/rad/rad/engine/reject"
)

// Engine executes reads and writes against the database's tables.
type Engine struct {
	store kv.TransactionalKV
	cat   *catalog.Catalog
	recur RecursionLimits

	schemaJobConfig      SchemaJobConfig
	schemaJobsEnabled    bool
	automaticIndexBuilds bool
	automaticReclamation bool
	schemaJobHooks       schemaJobHooks
	schemaJobs           *schemaJobRunner
	yieldHook            YieldHook
}

// RecursionLimits bounds a recursive binding's fixpoint. They are execution
// safeguards, not query semantics — a terminating recursion never reaches
// them — so tune them to the deepest legitimate hierarchy and the most rows a
// query may return, not to any property of the language.
type RecursionLimits struct {
	MaxIterations int // frontier rounds before the query fails
	MaxRows       int // accumulated rows before the query fails
}

const (
	defaultMaxRecursionIterations = 10000
	defaultMaxRecursionRows       = 1_000_000
)

// Option configures an Engine at construction.
type Option func(*Engine)

// WithRecursionLimits overrides the fixpoint safeguards; a zero field keeps
// its default.
func WithRecursionLimits(l RecursionLimits) Option {
	return func(e *Engine) {
		if l.MaxIterations > 0 {
			e.recur.MaxIterations = l.MaxIterations
		}
		if l.MaxRows > 0 {
			e.recur.MaxRows = l.MaxRows
		}
	}
}

// withAutomaticReclamation is an internal test seam. Production engines
// always schedule durable reclamation automatically after catalog commits and
// when opening over unfinished work.
func withAutomaticReclamation(enabled bool) Option {
	return func(e *Engine) { e.automaticReclamation = enabled }
}

// SchemaJobConfig bounds the local work runner. Batch, I/O-item, retry, and
// history-retention settings are process-local policy. Delta limits are copied
// into each newly started transition because they govern durable retained work
// and foreground write admission; changing an Engine option does not rewrite
// existing transitions.
type SchemaJobConfig struct {
	IndexBatchSize          int
	ReclamationBatchSize    int
	BatchesBeforeYield      int
	IOBudgetItemsPerYield   int
	YieldInterval           time.Duration
	RetryBackoffMin         time.Duration
	RetryBackoffMax         time.Duration
	MaxRetryAttempts        int
	CatalogHistoryRetain    uint64
	CatalogHistoryBatchSize int
	DeltaSoftLimit          uint64 // unapplied delta records before degraded
	DeltaHardLimit          uint64 // unapplied delta records before write gating
}

// WithSchemaJobConfig overrides positive scheduler resource limits. A zero
// field keeps the production default.
func WithSchemaJobConfig(config SchemaJobConfig) Option {
	return func(e *Engine) {
		if config.IndexBatchSize > 0 {
			e.schemaJobConfig.IndexBatchSize = config.IndexBatchSize
		}
		if config.ReclamationBatchSize > 0 {
			e.schemaJobConfig.ReclamationBatchSize = config.ReclamationBatchSize
		}
		if config.BatchesBeforeYield > 0 {
			e.schemaJobConfig.BatchesBeforeYield = config.BatchesBeforeYield
		}
		if config.IOBudgetItemsPerYield > 0 {
			e.schemaJobConfig.IOBudgetItemsPerYield = config.IOBudgetItemsPerYield
		}
		if config.YieldInterval > 0 {
			e.schemaJobConfig.YieldInterval = config.YieldInterval
		}
		if config.RetryBackoffMin > 0 {
			e.schemaJobConfig.RetryBackoffMin = config.RetryBackoffMin
		}
		if config.RetryBackoffMax > 0 {
			e.schemaJobConfig.RetryBackoffMax = config.RetryBackoffMax
		}
		if config.MaxRetryAttempts > 0 {
			e.schemaJobConfig.MaxRetryAttempts = config.MaxRetryAttempts
		}
		if config.CatalogHistoryRetain > 0 {
			e.schemaJobConfig.CatalogHistoryRetain = config.CatalogHistoryRetain
		}
		if config.CatalogHistoryBatchSize > 0 {
			e.schemaJobConfig.CatalogHistoryBatchSize = config.CatalogHistoryBatchSize
		}
		if config.DeltaSoftLimit > 0 {
			e.schemaJobConfig.DeltaSoftLimit = config.DeltaSoftLimit
		}
		if config.DeltaHardLimit > 0 {
			e.schemaJobConfig.DeltaHardLimit = config.DeltaHardLimit
		}
		switch {
		case config.DeltaSoftLimit > 0 && config.DeltaHardLimit == 0 &&
			e.schemaJobConfig.DeltaHardLimit <= e.schemaJobConfig.DeltaSoftLimit:
			if e.schemaJobConfig.DeltaSoftLimit == ^uint64(0) {
				e.schemaJobConfig.DeltaHardLimit = ^uint64(0)
			} else {
				e.schemaJobConfig.DeltaHardLimit = e.schemaJobConfig.DeltaSoftLimit + 1
			}
		case config.DeltaHardLimit > 0 && config.DeltaSoftLimit == 0 &&
			e.schemaJobConfig.DeltaSoftLimit >= e.schemaJobConfig.DeltaHardLimit:
			e.schemaJobConfig.DeltaSoftLimit = e.schemaJobConfig.DeltaHardLimit / 2
		}
	}
}

// WithSchemaJobScheduling controls whether this Engine instance participates
// in background schema work. Applications normally keep it enabled. It is
// useful for read-only diagnostic handles and tests that drive bounded worker
// primitives directly; disabling one handle does not alter durable job state.
func WithSchemaJobScheduling(enabled bool) Option {
	return func(e *Engine) { e.schemaJobsEnabled = enabled }
}

// WithYieldHook installs a synchronous semantic-boundary hook. It is intended
// for deterministic concurrency tests and is a no-op unless explicitly set.
func WithYieldHook(hook YieldHook) Option {
	return func(e *Engine) { e.yieldHook = hook }
}

func withAutomaticIndexBuilds(enabled bool) Option {
	return func(e *Engine) { e.automaticIndexBuilds = enabled }
}

func withSchemaJobHooks(hooks schemaJobHooks) Option {
	return func(e *Engine) { e.schemaJobHooks = hooks }
}

func New(database kv.TransactionalKV, cat *catalog.Catalog, opts ...Option) *Engine {
	e := &Engine{
		store:                database,
		cat:                  cat,
		schemaJobConfig:      defaultSchemaJobConfig(),
		schemaJobsEnabled:    true,
		automaticIndexBuilds: true,
		automaticReclamation: true,
		recur: RecursionLimits{
			MaxIterations: defaultMaxRecursionIterations,
			MaxRows:       defaultMaxRecursionRows,
		},
	}
	for _, o := range opts {
		o(e)
	}
	if e.schemaJobConfig.DeltaHardLimit <= e.schemaJobConfig.DeltaSoftLimit {
		if e.schemaJobConfig.DeltaSoftLimit == ^uint64(0) {
			e.schemaJobConfig.DeltaSoftLimit--
			e.schemaJobConfig.DeltaHardLimit = ^uint64(0)
		} else {
			e.schemaJobConfig.DeltaHardLimit = e.schemaJobConfig.DeltaSoftLimit + 1
		}
	}
	if e.schemaJobConfig.RetryBackoffMax < e.schemaJobConfig.RetryBackoffMin {
		e.schemaJobConfig.RetryBackoffMax = e.schemaJobConfig.RetryBackoffMin
	}
	e.schemaJobs = newSchemaJobRunner(e)
	cat.OnChange(e.kickSchemaJobs)
	if e.schemaJobsEnabled {
		transitions, transitionErr := store.HasTransitionHistory(context.Background(), database)
		reclamations, reclamationErr := store.HasReclamationHistory(context.Background(), database)
		history, historyErr := store.RevisionCompactionNeeded(
			context.Background(),
			database,
			e.schemaJobConfig.CatalogHistoryRetain,
		)
		if transitionErr != nil || reclamationErr != nil || historyErr != nil ||
			transitions || reclamations || history {
			e.kickSchemaJobs()
		}
	}
	return e
}

// Close stops this engine's local schema-job runner and waits for its current
// bounded transaction to return. It does not close the shared KV store and is
// safe to call more than once.
func (e *Engine) Close() error {
	if e.schemaJobs != nil {
		e.schemaJobs.close()
	}
	return nil
}

// Tx is a transaction-scoped view of the engine. All reads see the snapshot
// taken at Begin plus the transaction's own writes; all writes are buffered
// until the enclosing Engine.Txn commits.
type Tx struct {
	e            *Engine
	txn          kv.Txn
	catalog      *catalogSnapshot
	catalogDirty bool
}

// Txn runs fn inside a SerializableSnapshot transaction and commits it if fn
// returns nil. A commit-time concurrency race returns an error wrapping
// kv.ErrConflict; the caller can retry by calling Txn again (fn must be safe
// to re-run). If fn returns an error the transaction is rolled back.
func (e *Engine) Txn(ctx context.Context, fn func(tx *Tx) error) error {
	tx, err := e.Begin(ctx)
	if err != nil {
		return err
	}
	defer tx.Rollback()
	if err := fn(tx); err != nil {
		return err
	}
	return tx.Commit(ctx)
}

// CatalogTxn runs a group of catalog operations and their associated data
// work as one serializable transaction and records one catalog revision when
// the group changes the schema.
func (e *Engine) CatalogTxn(ctx context.Context, fn func(tx *Tx, change *change.Mutation) error) error {
	return e.Txn(ctx, func(tx *Tx) error {
		_, err := change.Apply(ctx, tx.txn, func(change *change.Mutation) error {
			return fn(tx, change)
		})
		if err == nil {
			tx.catalogDirty = true
			e.yield(ctx, YieldCatalogPublicationPrepared, "")
		}
		return err
	})
}

// Begin starts an explicit transaction. Prefer Txn where a callback fits;
// Begin exists for drivers (like the HTTP server) that hold a transaction
// open across requests. The caller must Commit or Rollback.
func (e *Engine) Begin(ctx context.Context) (*Tx, error) {
	return e.begin(ctx, kv.SerializableSnapshot)
}

func (e *Engine) begin(ctx context.Context, level kv.IsolationLevel) (*Tx, error) {
	pinned, err := pinCatalog(ctx, e.store)
	if err != nil {
		return nil, err
	}
	e.yield(ctx, YieldCatalogPinned, "")
	txn, err := e.store.Begin(ctx, level)
	if err != nil {
		return nil, err
	}
	e.yield(ctx, YieldSnapshotBegun, "")
	return &Tx{e: e, txn: txn, catalog: pinned}, nil
}

// Commit atomically applies the transaction's writes. The Tx is unusable
// afterwards.
func (tx *Tx) Commit(ctx context.Context) error {
	tx.e.yield(ctx, YieldCommitReady, "")
	err := tx.txn.Commit(ctx)
	if err == nil && tx.catalogDirty {
		tx.e.kickSchemaJobs()
	}
	if err == nil {
		tx.e.yield(ctx, YieldTransactionCommitted, "")
	}
	return err
}

// Rollback discards the transaction; safe to call after Commit.
func (tx *Tx) Rollback() error { return tx.txn.Rollback() }

// IsConflict reports whether err is a transaction conflict that can be
// resolved by retrying the whole Engine.Txn.
func IsConflict(err error) bool { return errors.Is(err, kv.ErrConflict) }

func (e *Engine) Catalog() *catalog.Catalog { return e.cat }

// tableIn resolves a table through the statement's KV view, so schema
// resolution shares the snapshot of the data reads and writes that follow —
// and, inside a serializable transaction, joins its read set.
func tableIn(ctx context.Context, view kv.KV, name string) (model.Table, error) {
	tbl, ok, err := store.New(view).GetTable(ctx, name)
	if err != nil {
		return model.Table{}, err
	}
	if !ok {
		return model.Table{}, reject.Inputf("exec: table %q does not exist", name)
	}
	return tbl, nil
}

// Insert adds one row in its own transaction. For multi-row atomicity use
// Engine.Txn with Tx.Insert.
func (e *Engine) Insert(ctx context.Context, table string, row lir.Row) error {
	_, err := e.Create(ctx, table, row)
	return err
}

func (tx *Tx) Insert(ctx context.Context, table string, row lir.Row) error {
	_, err := tx.Create(ctx, table, row)
	return err
}

// Create is Insert returning the stored row — the caller's values plus
// applied defaults (generated IDs, timestamps).
func (e *Engine) Create(ctx context.Context, table string, row lir.Row) (lir.Row, error) {
	var stored lir.Row
	err := e.Txn(ctx, func(tx *Tx) error {
		var err error
		stored, err = tx.Create(ctx, table, row)
		return err
	})
	if err != nil {
		return nil, err
	}
	return stored, nil
}

func (tx *Tx) Create(ctx context.Context, table string, row lir.Row) (lir.Row, error) {
	tbl, err := tx.table(ctx, table)
	if err != nil {
		return nil, err
	}
	tx.e.yield(ctx, YieldBindingResolved, tbl.Name)
	stored, err := mutate.CreateOne(ctx, tx.txn, tbl, row)
	if err == nil {
		tx.e.yield(ctx, YieldDependencyFencesAdmitted, tbl.Name)
	}
	return stored, err
}

// GetByPrimaryKey fetches one row by its primary key values. key must contain
// exactly the primary key columns.
func (e *Engine) GetByPrimaryKey(ctx context.Context, table string, key lir.Row) (lir.Row, bool, error) {
	txn, err := e.store.Begin(ctx, kv.Snapshot)
	if err != nil {
		return nil, false, err
	}
	defer txn.Rollback()
	return getByPrimaryKey(ctx, txn, table, key)
}

func (tx *Tx) GetByPrimaryKey(ctx context.Context, table string, key lir.Row) (lir.Row, bool, error) {
	tbl, err := tx.table(ctx, table)
	if err != nil {
		return nil, false, err
	}
	row, _, ok, err := rowstore.Get(ctx, tx.txn, tbl, key)
	if err == nil {
		tx.e.yield(ctx, YieldDependencyFencesAdmitted, tbl.Name)
	}
	return row, ok, err
}

func getByPrimaryKey(ctx context.Context, view kv.KV, table string, key lir.Row) (lir.Row, bool, error) {
	tbl, err := tableIn(ctx, view, table)
	if err != nil {
		return nil, false, err
	}
	row, _, ok, err := rowstore.Get(ctx, view, tbl, key)
	return row, ok, err
}

// RowIterator streams rows from a scan.
type RowIterator interface {
	// Next returns the next row, or ok=false when the scan is exhausted.
	Next() (lir.Row, bool, error)
	Close() error
}

// ScanTable streams every row of the table in primary key order, from a
// snapshot held until the iterator is closed.
func (e *Engine) ScanTable(ctx context.Context, table string) (RowIterator, error) {
	txn, err := e.store.Begin(ctx, kv.Snapshot)
	if err != nil {
		return nil, err
	}
	tbl, err := tableIn(ctx, txn, table)
	if err != nil {
		txn.Rollback()
		return nil, err
	}
	it, err := rowstore.ScanTable(ctx, txn, tbl)
	if err != nil {
		txn.Rollback()
		return nil, err
	}
	return &snapshotRowIterator{RowIterator: it, txn: txn}, nil
}

// snapshotRowIterator ties a statement-scoped snapshot's lifetime to the
// iterator it feeds.
type snapshotRowIterator struct {
	RowIterator
	txn kv.Txn
}

func (s *snapshotRowIterator) Close() error {
	err := s.RowIterator.Close()
	if rerr := s.txn.Rollback(); err == nil {
		err = rerr
	}
	return err
}

// ScanTable inside a transaction sees the snapshot plus the transaction's
// own writes. The iterator must be closed before the transaction commits.
func (tx *Tx) ScanTable(ctx context.Context, table string) (RowIterator, error) {
	tbl, err := tx.table(ctx, table)
	if err != nil {
		return nil, err
	}
	it, err := rowstore.ScanTable(ctx, tx.txn, tbl)
	if err == nil {
		tx.e.yield(ctx, YieldDependencyFencesAdmitted, tbl.Name)
	}
	return it, err
}
