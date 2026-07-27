package exec

import (
	"context"
	"errors"
	"fmt"
	"sort"
	"sync"
	"time"

	kv "github.com/Southclaws/rad/rad/engine/01_kv"
	"github.com/Southclaws/rad/rad/engine/02_catalog/model"
	"github.com/Southclaws/rad/rad/engine/02_catalog/store"
)

const (
	defaultSchemaJobBatchesBeforeYield = 16
	defaultSchemaJobIOBudget           = 2048
	defaultSchemaJobMaxRetryAttempts   = 8
	defaultCatalogHistoryRetain        = 256
	defaultCatalogHistoryBatchSize     = 128
)

const (
	defaultSchemaJobRetryBackoffMin = 5 * time.Millisecond
	defaultSchemaJobRetryBackoffMax = time.Second
)

func defaultSchemaJobConfig() SchemaJobConfig {
	return SchemaJobConfig{
		IndexBatchSize:          defaultIndexBuildBatchSize,
		ReclamationBatchSize:    defaultReclamationBatchSize,
		BatchesBeforeYield:      defaultSchemaJobBatchesBeforeYield,
		IOBudgetItemsPerYield:   defaultSchemaJobIOBudget,
		YieldInterval:           time.Millisecond,
		RetryBackoffMin:         defaultSchemaJobRetryBackoffMin,
		RetryBackoffMax:         defaultSchemaJobRetryBackoffMax,
		MaxRetryAttempts:        defaultSchemaJobMaxRetryAttempts,
		CatalogHistoryRetain:    defaultCatalogHistoryRetain,
		CatalogHistoryBatchSize: defaultCatalogHistoryBatchSize,
		DeltaSoftLimit:          model.DefaultDeltaSoftLimit,
		DeltaHardLimit:          model.DefaultDeltaHardLimit,
	}
}

type schemaJobKind string

const (
	schemaJobIndexBuild           schemaJobKind = "index_build"
	schemaJobColumnReplacement    schemaJobKind = "column_replacement"
	schemaJobConstraintValidation schemaJobKind = "constraint_validation"
	schemaJobReclamation          schemaJobKind = "reclamation"
	schemaJobTransitionCompaction schemaJobKind = "transition_compaction"
	schemaJobCatalogHistory       schemaJobKind = "catalog_history"
)

type schemaJob struct {
	kind schemaJobKind
	id   string
}

func (j schemaJob) key() string { return string(j.kind) + ":" + j.id }

type schemaJobHooks struct {
	afterBatch func(context.Context, schemaJobKind, string)
}

type schemaJobRetry struct {
	backoffStep int
	failures    int
	next        time.Time
}

type schemaJobRunnerCounters struct {
	Batches  uint64
	Items    uint64
	Backoffs uint64
}

// schemaJobRunner is process-local scheduling policy over durable catalog
// records. It owns no correctness state: transition/reclamation checkpoints,
// ownership epochs, and physical writes remain atomically committed in Slate.
type schemaJobRunner struct {
	engine *Engine
	config SchemaJobConfig

	ctx    context.Context
	cancel context.CancelFunc

	mu      sync.Mutex
	running bool
	pending bool
	closed  bool
	wg      sync.WaitGroup

	next              int
	transitionOwners  map[string]uint64
	reclamationOwners map[string]uint64
	blocked           map[string]struct{}
	retries           map[string]schemaJobRetry
	counters          schemaJobRunnerCounters
}

func newSchemaJobRunner(engine *Engine) *schemaJobRunner {
	ctx, cancel := context.WithCancel(context.Background())
	return &schemaJobRunner{
		engine:            engine,
		config:            engine.schemaJobConfig,
		ctx:               ctx,
		cancel:            cancel,
		transitionOwners:  make(map[string]uint64),
		reclamationOwners: make(map[string]uint64),
		blocked:           make(map[string]struct{}),
		retries:           make(map[string]schemaJobRetry),
	}
}

func (e *Engine) kickSchemaJobs() {
	if !e.schemaJobsEnabled || e.schemaJobs == nil {
		return
	}
	e.schemaJobs.kick()
}

func (r *schemaJobRunner) kick() {
	r.mu.Lock()
	defer r.mu.Unlock()
	if r.closed {
		return
	}
	if r.running {
		r.pending = true
		return
	}
	r.running = true
	r.wg.Add(1)
	go r.run()
}

func (r *schemaJobRunner) close() {
	r.mu.Lock()
	if !r.closed {
		r.closed = true
		r.cancel()
	}
	r.mu.Unlock()
	r.wg.Wait()
}

func (r *schemaJobRunner) run() {
	defer r.wg.Done()
	r.blocked = make(map[string]struct{})
	for {
		active, progressed, err := r.runRound(r.ctx)
		if err != nil || !active {
			r.mu.Lock()
			if r.pending && !r.closed {
				r.pending = false
				r.blocked = make(map[string]struct{})
				r.retries = make(map[string]schemaJobRetry)
				r.mu.Unlock()
				continue
			}
			r.running = false
			r.mu.Unlock()
			return
		}
		delay := r.config.YieldInterval
		if !progressed {
			delay = r.retryDelay(delay)
		}
		if !waitSchemaJobYield(r.ctx, delay) {
			r.mu.Lock()
			r.running = false
			r.mu.Unlock()
			return
		}
	}
}

func waitSchemaJobYield(ctx context.Context, interval time.Duration) bool {
	if interval <= 0 {
		select {
		case <-ctx.Done():
			return false
		default:
			return true
		}
	}
	timer := time.NewTimer(interval)
	defer timer.Stop()
	select {
	case <-ctx.Done():
		return false
	case <-timer.C:
		return true
	}
}

// runRound discovers durable work afresh, then advances a bounded rotating
// window. Each selected job receives at most one batch, so a large backfill or
// table reclamation cannot monopolise the runner.
func (r *schemaJobRunner) runRound(ctx context.Context) (active, progressed bool, err error) {
	jobs, err := r.listJobs(ctx)
	if err != nil || len(jobs) == 0 {
		return false, false, err
	}
	batchBudget := r.config.BatchesBeforeYield
	if batchBudget <= 0 || batchBudget > len(jobs) {
		batchBudget = len(jobs)
	}
	itemBudget := r.config.IOBudgetItemsPerYield
	if itemBudget <= 0 {
		itemBudget = defaultSchemaJobIOBudget
	}
	if r.next >= len(jobs) {
		r.next = 0
	}
	visited := 0
	for offset := 0; offset < batchBudget && itemBudget > 0; offset++ {
		visited++
		position := (r.next + offset) % len(jobs)
		items, stepErr := r.advance(ctx, jobs[position], itemBudget)
		if stepErr != nil {
			return true, progressed, stepErr
		}
		if items > 0 {
			progressed = true
			itemBudget -= min(items, itemBudget)
			r.addCounters(items, false)
		}
	}
	r.next = (r.next + visited) % len(jobs)
	return true, progressed, nil
}

func (r *schemaJobRunner) listJobs(ctx context.Context) ([]schemaJob, error) {
	var jobs []schemaJob
	transitions, err := store.ListTransitions(ctx, r.engine.store)
	if err != nil {
		return nil, err
	}
	for _, transition := range transitions {
		var job schemaJob
		switch transition.Kind {
		case model.TransitionIndexBuild:
			if !r.engine.automaticIndexBuilds {
				continue
			}
			job = schemaJob{kind: schemaJobIndexBuild, id: transition.ID}
		case model.TransitionColumnReplacement:
			job = schemaJob{kind: schemaJobColumnReplacement, id: transition.ID}
		case model.TransitionConstraintValidation:
			job = schemaJob{kind: schemaJobConstraintValidation, id: transition.ID}
		default:
			continue
		}
		if _, blocked := r.blocked[job.key()]; blocked {
			continue
		}
		switch transition.State {
		case model.TransitionWaiting, model.TransitionBuilding, model.TransitionCatchingUp, model.TransitionValidating:
			jobs = append(jobs, job)
		}
	}
	if r.engine.automaticReclamation {
		reclamations, err := store.ListReclamations(ctx, r.engine.store)
		if err != nil {
			return nil, err
		}
		for _, reclamation := range reclamations {
			if _, blocked := r.blocked[(schemaJob{kind: schemaJobReclamation, id: reclamation.ID}).key()]; blocked {
				continue
			}
			if reclamation.State == model.ReclamationPending || reclamation.State == model.ReclamationReclaiming {
				jobs = append(jobs, schemaJob{kind: schemaJobReclamation, id: reclamation.ID})
			}
		}
		for _, transition := range transitions {
			eligible, err := store.TransitionCompactionEligible(ctx, r.engine.store, transition)
			if err != nil {
				return nil, err
			}
			if eligible {
				jobs = append(jobs, schemaJob{kind: schemaJobTransitionCompaction, id: transition.ID})
			}
		}
	}
	if r.config.CatalogHistoryRetain > 0 {
		needed, err := store.RevisionCompactionNeeded(ctx, r.engine.store, r.config.CatalogHistoryRetain)
		if err != nil {
			return nil, err
		}
		if needed {
			jobs = append(jobs, schemaJob{kind: schemaJobCatalogHistory, id: "canonical-revisions"})
		}
	}
	sort.Slice(jobs, func(i, j int) bool { return jobs[i].key() < jobs[j].key() })
	return jobs, nil
}

func (r *schemaJobRunner) advance(ctx context.Context, job schemaJob, itemBudget int) (int, error) {
	if retry, ok := r.retries[job.key()]; ok && time.Now().Before(retry.next) {
		return 0, nil
	}
	switch job.kind {
	case schemaJobIndexBuild:
		ctx = WithYieldActor(ctx, "index-worker")
	case schemaJobColumnReplacement:
		ctx = WithYieldActor(ctx, "replacement-worker")
	case schemaJobConstraintValidation:
		ctx = WithYieldActor(ctx, "constraint-worker")
	}
	switch job.kind {
	case schemaJobIndexBuild, schemaJobColumnReplacement, schemaJobConstraintValidation:
		transition, err := r.engine.inspectSchemaTransition(ctx, job.id)
		if err != nil {
			return 0, err
		}
		if transition.State == model.TransitionWaiting {
			activated, err := r.engine.activateWaitingSchemaTransition(ctx, job.id)
			if err != nil {
				if errors.Is(err, kv.ErrConflict) {
					r.deferRetry(job, false)
					return 0, nil
				}
				return 0, err
			}
			if activated.State == model.TransitionWaiting {
				r.deferRetry(job, false)
				return 0, nil
			}
			delete(r.retries, job.key())
			return 1, nil
		}
	}
	var items int
	var err error
	switch job.kind {
	case schemaJobIndexBuild:
		items, err = r.advanceIndexBuild(ctx, job, itemBudget)
	case schemaJobColumnReplacement:
		items, err = r.advanceColumnReplacement(ctx, job, itemBudget)
	case schemaJobConstraintValidation:
		items, err = r.advanceConstraintValidation(ctx, job, itemBudget)
	case schemaJobReclamation:
		items, err = r.advanceReclamation(ctx, job, itemBudget)
	case schemaJobTransitionCompaction:
		items, err = r.advanceTransitionCompaction(ctx, job)
	case schemaJobCatalogHistory:
		items, err = r.advanceCatalogHistory(ctx, itemBudget)
	default:
		err = fmt.Errorf("exec: unknown schema job kind %q", job.kind)
	}
	if items > 0 {
		delete(r.retries, job.key())
	}
	if items > 0 && r.engine.schemaJobHooks.afterBatch != nil {
		r.engine.schemaJobHooks.afterBatch(ctx, job.kind, job.id)
	}
	return items, err
}

func (r *schemaJobRunner) advanceIndexBuild(ctx context.Context, job schemaJob, itemBudget int) (int, error) {
	id := job.id
	owner, ok := r.transitionOwners[id]
	if !ok {
		claimed, err := r.engine.claimIndexBuild(ctx, id)
		if err != nil {
			if errors.Is(err, kv.ErrConflict) {
				r.deferRetry(job, false)
				return 0, nil
			}
			return r.handleTransitionError(ctx, job, owner, err)
		}
		owner = claimed
		r.transitionOwners[id] = owner
	}
	before, err := r.engine.inspectSchemaTransition(ctx, id)
	if err != nil {
		return r.handleTransitionError(ctx, job, owner, err)
	}
	batchSize := min(r.config.IndexBatchSize, itemBudget)
	transition, err := r.engine.stepIndexBuild(ctx, id, owner, batchSize)
	if err != nil {
		if errors.Is(err, kv.ErrConflict) {
			current, inspectErr := r.engine.inspectSchemaTransition(ctx, id)
			if inspectErr != nil {
				return 0, inspectErr
			}
			if current.OwnerEpoch != owner {
				delete(r.transitionOwners, id)
			}
			r.deferRetry(job, false)
			return 0, nil
		}
		if errors.Is(err, context.Canceled) || errors.Is(err, context.DeadlineExceeded) {
			return 0, err
		}
		return r.handleTransitionError(ctx, job, owner, err)
	}
	if transition.State == model.TransitionReady || transition.State == model.TransitionFailed || transition.State == model.TransitionCancelled {
		delete(r.transitionOwners, id)
	}
	rows, err := schemaJobProgress("rows scanned", before.RowsScanned, transition.RowsScanned)
	if err != nil {
		return 0, err
	}
	deltas, err := schemaJobProgress("applied delta", before.AppliedDelta, transition.AppliedDelta)
	if err != nil {
		return 0, err
	}
	if rows > int(^uint(0)>>1)-deltas {
		return 0, fmt.Errorf("exec: index build %q progress exceeds scheduler accounting range", id)
	}
	items := rows + deltas
	if items == 0 {
		items = 1
	}
	return items, nil
}

func (r *schemaJobRunner) advanceReclamation(ctx context.Context, job schemaJob, itemBudget int) (int, error) {
	id := job.id
	owner, ok := r.reclamationOwners[id]
	if !ok {
		claimedOwner, claimed, err := r.engine.claimReclamation(ctx, id)
		if err != nil {
			if errors.Is(err, kv.ErrConflict) {
				r.deferRetry(job, false)
				return 0, nil
			}
			return 0, err
		}
		if !claimed {
			return 0, nil
		}
		owner = claimedOwner
		r.reclamationOwners[id] = owner
	}
	before, exists, err := store.GetReclamation(ctx, r.engine.store, id)
	if err != nil || !exists {
		return 0, err
	}
	batchSize := min(r.config.ReclamationBatchSize, itemBudget)
	reclamation, err := r.engine.stepReclamation(ctx, id, owner, batchSize)
	if err != nil {
		if errors.Is(err, kv.ErrConflict) {
			current, exists, inspectErr := store.GetReclamation(ctx, r.engine.store, id)
			if inspectErr != nil {
				return 0, inspectErr
			}
			if !exists || current.OwnerEpoch != owner {
				delete(r.reclamationOwners, id)
			}
			r.deferRetry(job, false)
			return 0, nil
		}
		if errors.Is(err, context.Canceled) || errors.Is(err, context.DeadlineExceeded) {
			return 0, err
		}
		if errors.Is(err, ErrRetentionPinned) {
			r.deferRetry(job, false)
			return 0, nil
		}
		if !r.deferRetry(job, true) {
			return 0, nil
		}
		_ = r.engine.failReclamation(context.Background(), id, owner, err)
		delete(r.reclamationOwners, id)
		r.blocked[job.key()] = struct{}{}
		return 0, nil
	}
	if reclamation.State == model.ReclamationReclaimed {
		delete(r.reclamationOwners, id)
	}
	items, err := schemaJobProgress("items reclaimed", before.ItemsReclaimed, reclamation.ItemsReclaimed)
	if err != nil {
		return 0, err
	}
	if items == 0 {
		items = 1
	}
	return items, nil
}

func (r *schemaJobRunner) advanceColumnReplacement(
	ctx context.Context,
	job schemaJob,
	itemBudget int,
) (int, error) {
	owner, ok := r.transitionOwners[job.id]
	if !ok {
		claimed, err := r.engine.claimColumnReplacement(ctx, job.id)
		if err != nil {
			if errors.Is(err, kv.ErrConflict) {
				r.deferRetry(job, false)
				return 0, nil
			}
			return r.handleTransitionError(ctx, job, owner, err)
		}
		owner = claimed
		r.transitionOwners[job.id] = owner
	}
	before, err := r.engine.inspectSchemaTransition(ctx, job.id)
	if err != nil {
		return r.handleTransitionError(ctx, job, owner, err)
	}
	transition, err := r.engine.stepColumnReplacement(
		ctx,
		job.id,
		owner,
		min(r.config.IndexBatchSize, itemBudget),
	)
	if err != nil {
		if errors.Is(err, kv.ErrConflict) {
			current, inspectErr := r.engine.inspectSchemaTransition(ctx, job.id)
			if inspectErr != nil {
				return 0, inspectErr
			}
			if current.OwnerEpoch != owner {
				delete(r.transitionOwners, job.id)
			}
			r.deferRetry(job, false)
			return 0, nil
		}
		if errors.Is(err, context.Canceled) || errors.Is(err, context.DeadlineExceeded) {
			return 0, err
		}
		return r.handleTransitionError(ctx, job, owner, err)
	}
	switch transition.State {
	case model.TransitionReady, model.TransitionFailed, model.TransitionCancelled:
		delete(r.transitionOwners, job.id)
	}
	items, err := schemaJobProgress("rows scanned", before.RowsScanned, transition.RowsScanned)
	if err != nil {
		return 0, err
	}
	if items == 0 {
		items = 1
	}
	return items, nil
}

func (r *schemaJobRunner) advanceConstraintValidation(
	ctx context.Context,
	job schemaJob,
	itemBudget int,
) (int, error) {
	owner, ok := r.transitionOwners[job.id]
	if !ok {
		claimed, err := r.engine.claimConstraintValidation(ctx, job.id)
		if err != nil {
			if errors.Is(err, kv.ErrConflict) {
				r.deferRetry(job, false)
				return 0, nil
			}
			return r.handleTransitionError(ctx, job, owner, err)
		}
		owner = claimed
		r.transitionOwners[job.id] = owner
	}
	before, err := r.engine.inspectSchemaTransition(ctx, job.id)
	if err != nil {
		return r.handleTransitionError(ctx, job, owner, err)
	}
	transition, err := r.engine.stepConstraintValidation(
		ctx,
		job.id,
		owner,
		min(r.config.IndexBatchSize, itemBudget),
	)
	if err != nil {
		if errors.Is(err, kv.ErrConflict) {
			current, inspectErr := r.engine.inspectSchemaTransition(ctx, job.id)
			if inspectErr != nil {
				return 0, inspectErr
			}
			if current.OwnerEpoch != owner {
				delete(r.transitionOwners, job.id)
			}
			r.deferRetry(job, false)
			return 0, nil
		}
		if errors.Is(err, context.Canceled) || errors.Is(err, context.DeadlineExceeded) {
			return 0, err
		}
		return r.handleTransitionError(ctx, job, owner, err)
	}
	switch transition.State {
	case model.TransitionReady, model.TransitionFailed, model.TransitionCancelled:
		delete(r.transitionOwners, job.id)
	}
	items, err := schemaJobProgress("rows scanned", before.RowsScanned, transition.RowsScanned)
	if err != nil {
		return 0, err
	}
	if items == 0 {
		items = 1
	}
	return items, nil
}

func (r *schemaJobRunner) handleTransitionError(
	ctx context.Context,
	job schemaJob,
	owner uint64,
	cause error,
) (int, error) {
	if job.kind == schemaJobIndexBuild {
		_ = r.engine.recordIndexBuildError(ctx, job.id, owner, cause)
	} else {
		_ = r.engine.recordSchemaTransitionError(ctx, job.id, owner, cause)
	}
	if !r.deferRetry(job, true) {
		return 0, nil
	}
	delete(r.transitionOwners, job.id)
	r.blocked[job.key()] = struct{}{}
	return 0, nil
}

func (r *schemaJobRunner) advanceTransitionCompaction(ctx context.Context, job schemaJob) (int, error) {
	txn, err := r.engine.store.Begin(ctx, kv.SerializableSnapshot)
	if err != nil {
		return 0, err
	}
	defer txn.Rollback()
	transition, ok, err := store.GetTransition(ctx, txn, job.id)
	if err != nil || !ok {
		return 0, err
	}
	eligible, err := store.TransitionCompactionEligible(ctx, txn, transition)
	if err != nil || !eligible {
		return 0, err
	}
	if err := store.CompactTransition(ctx, txn, job.id, time.Now().UTC()); err != nil {
		return 0, err
	}
	if err := txn.Commit(ctx); err != nil {
		if errors.Is(err, kv.ErrConflict) {
			r.deferRetry(job, false)
			return 0, nil
		}
		return 0, err
	}
	return 1, nil
}

func (r *schemaJobRunner) advanceCatalogHistory(ctx context.Context, itemBudget int) (int, error) {
	txn, err := r.engine.store.Begin(ctx, kv.SerializableSnapshot)
	if err != nil {
		return 0, err
	}
	defer txn.Rollback()
	batchSize := min(r.config.CatalogHistoryBatchSize, itemBudget)
	deleted, _, err := store.CompactRevisionHistoryBatch(
		ctx,
		txn,
		r.config.CatalogHistoryRetain,
		batchSize,
	)
	if err != nil || deleted == 0 {
		return deleted, err
	}
	if err := txn.Commit(ctx); err != nil {
		if errors.Is(err, kv.ErrConflict) {
			r.deferRetry(schemaJob{kind: schemaJobCatalogHistory, id: "canonical-revisions"}, false)
			return 0, nil
		}
		return 0, err
	}
	return deleted, nil
}

// deferRetry returns true when a counted failure exhausted its retry budget
// and the caller should quarantine the durable job. Expected contention and
// retention waits use counted=false: their exponential delay is capped but
// they never become terminal failures.
func (r *schemaJobRunner) deferRetry(job schemaJob, counted bool) bool {
	retry := r.retries[job.key()]
	if counted {
		retry.failures++
		if retry.failures >= r.config.MaxRetryAttempts {
			delete(r.retries, job.key())
			return true
		}
	}
	if retry.backoffStep < r.config.MaxRetryAttempts {
		retry.backoffStep++
	}
	exponent := retry.backoffStep - 1
	if exponent < 0 {
		exponent = 0
	}
	delay := r.config.RetryBackoffMin
	for range exponent {
		if delay >= r.config.RetryBackoffMax/2 {
			delay = r.config.RetryBackoffMax
			break
		}
		delay *= 2
	}
	if delay > r.config.RetryBackoffMax {
		delay = r.config.RetryBackoffMax
	}
	retry.next = time.Now().Add(delay)
	r.retries[job.key()] = retry
	r.addCounters(0, true)
	return false
}

func (r *schemaJobRunner) retryDelay(fallback time.Duration) time.Duration {
	now := time.Now()
	var earliest time.Duration
	for _, retry := range r.retries {
		delay := retry.next.Sub(now)
		if delay <= 0 {
			continue
		}
		if earliest == 0 || delay < earliest {
			earliest = delay
		}
	}
	if earliest > fallback {
		return earliest
	}
	return fallback
}

func (r *schemaJobRunner) addCounters(items int, backoff bool) {
	r.mu.Lock()
	defer r.mu.Unlock()
	if items > 0 {
		r.counters.Batches++
		r.counters.Items += uint64(items)
	}
	if backoff {
		r.counters.Backoffs++
	}
}

func (r *schemaJobRunner) readCounters() schemaJobRunnerCounters {
	r.mu.Lock()
	defer r.mu.Unlock()
	return r.counters
}

func schemaJobProgress(name string, before, after uint64) (int, error) {
	if after < before {
		return 0, fmt.Errorf("exec: schema job %s regressed from %d to %d", name, before, after)
	}
	delta := after - before
	maxInt := uint64(^uint(0) >> 1)
	if delta > maxInt {
		return 0, fmt.Errorf("exec: schema job %s progress %d exceeds scheduler accounting range", name, delta)
	}
	return int(delta), nil
}
