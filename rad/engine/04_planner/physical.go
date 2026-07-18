package planner

// The physical plan: what the executor runs. Logical bound IR names no
// indexes and no keys; these nodes do. The structural invariant: an access
// node only narrows which keys are scanned, and the FULL original predicate
// rides above it in a FilterExec — access-path choice can never change
// results.
//
// Ordering is a physical property here: scans declare what order they
// provide (ascending only — the KV has no reverse scan), and a SortExec
// exists only when the required ordering isn't already provided. Slices are
// lazy pull operators, so "stop early" is not a plan annotation — it is
// what a SliceExec over a non-blocking pipeline naturally does.

import (
	catalog "github.com/Southclaws/rad/rad/engine/02_catalog"
	lir "github.com/Southclaws/rad/rad/engine/03_lir"
	"github.com/Southclaws/rad/rad/engine/03_lir/bound"
)

// PhysNode is the sealed physical operator interface.
type PhysNode interface{ phys() }

// PhysPlan is a planned query: the operator tree, the root materialisation
// cardinality, and the output row type — plus the bindings, each lowered
// once, in dependency order.
type PhysPlan struct {
	Bindings []BindingPlan
	Root     PhysNode
	Card     lir.RootCard
	Out      lir.RowType
	// Slots is the high-water slot after planning: the binder's slots plus
	// the fresh slots the planner allocated for extracted crossings. A PIR
	// program threads this into the next statement's binding so the whole
	// program shares one dense slot space.
	Slots lir.SlotID
}

// BindingStrategy is how a binding's commitment is physically discharged.
type BindingStrategy string

const (
	// BindingMaterialise evaluates the plan once, before the root runs, and
	// every occurrence streams the stored frames. Required whenever more
	// than one occurrence exists — re-execution would be wasted work at
	// best (and, for a sensitive body under a diverged plan, a different
	// commitment at worst).
	BindingMaterialise BindingStrategy = "materialise"
	// BindingReplay streams the plan inline at the occurrence. Sound only
	// for a single reference: one occurrence's evaluation IS the
	// commitment, so nothing else can observe a difference.
	BindingReplay BindingStrategy = "replay"
)

// BindingPlan is one binding's body lowered once. Materialised bindings
// commit in list order (dependencies first) before the root runs; replayed
// bindings commit at their single occurrence's pull.
type BindingPlan struct {
	Name     string
	Plan     PhysNode
	Out      lir.RowType // canonical output
	Strategy BindingStrategy
	// Sensitive: plan-choice-sensitive body — the property that forbids
	// per-occurrence plan divergence, kept visible in EXPLAIN.
	Sensitive bool
	// Recursive marks a fixpoint binding. Plan is then the anchor; Step is the
	// inductive plan (containing a RecursiveRefExec that reads the frontier),
	// StepOut its output row type (for remapping onto canonical slots), and
	// Accumulation the admission policy. Recursive bindings are always
	// materialised.
	Recursive    bool
	Step         PhysNode
	StepOut      lir.RowType
	Accumulation lir.RecursiveAccumulationMode
}

// AccessDecision records why the planner picked an access path for a scan:
// the candidates it considered and their scores, with the winner marked. It is
// observability metadata hung on the chosen node — the query-plan view renders
// it; execution ignores it.
type AccessDecision struct {
	Candidates []AccessCandidate
}

// AccessCandidate is one access path the planner scored.
type AccessCandidate struct {
	Method string `json:"method"` // "TableScan" | "PKGet" | "IndexRangeScan <index>"
	Score  int    `json:"score"`
	Chosen bool   `json:"chosen,omitempty"`
}

// PKGetExec fetches at most one row by primary key. Key values may be
// literals or outer-slot parameters resolved from the environment when the
// operator is built — the deduplicated to-parent pattern is a PKGetExec per
// distinct key.
type PKGetExec struct {
	Scan   *bound.Scan
	Key    []ConstVal // one per primary-key column, in key order
	Access *AccessDecision
}

// TableScanExec reads every row of a table, in primary-key order.
type TableScanExec struct {
	Scan   *bound.Scan
	Access *AccessDecision
}

// RowsExec streams a constant relation's literal rows. No storage is
// touched; the values were coerced at bind time.
type RowsExec struct {
	Rows *bound.Rows
}

// RangeSpec bounds one index column right after the equality prefix.
type RangeSpec struct {
	Column string
	Lo, Hi *RangeBound
}

// IndexRangeScanExec scans an index by equality prefix plus an optional
// trailing range, fetching base rows as it goes. It provides the index's
// remaining column order, then the primary key — ascending.
type IndexRangeScanExec struct {
	Scan     *bound.Scan
	Index    catalog.Index
	EqPrefix []ConstVal // one per leading index column
	Range    *RangeSpec
	Access   *AccessDecision
}

// FilterExec keeps rows where the predicate is TRUE. The predicate is
// always the full original conjunction — the structural residual.
type FilterExec struct {
	Input PhysNode
	Pred  bound.Expr
}

// CrossKind names which crossing a field materialises.
type CrossKind string

const (
	CrossExists CrossKind = "exists"
	CrossFirst  CrossKind = "first"
	CrossScalar CrossKind = "scalar"
	CrossArray  CrossKind = "array"
)

// AttachSpec materialises one extracted crossing into a slot. The planner
// pulls every crossing out of every expression — projection fields, filter
// predicates, order terms, aggregate arguments — so expressions stay pure
// and the optimizer sees every sub-relation. The correlation classification
// decides the strategy: key-correlated attaches execute once per DISTINCT
// outer key over the batch (the deduplicated correlated execution);
// uncorrelated attaches execute once; general correlation falls back to
// per-row nested evaluation. All three are result-equivalent — wrapping a
// crossing in a wider expression cannot change how it executes.
type AttachSpec struct {
	Slot lir.SlotID // the slot the crossing's result is written to
	Kind CrossKind
	Corr Correlation
	Plan PhysNode
	Out  lir.RowType // the sub-relation's output shape
}

// AttachExec computes attach slots over its input stream, order-preserving:
// it drains the batch, executes each spec by its strategy, writes results
// into the frames, and re-emits them in input order.
type AttachExec struct {
	Input PhysNode
	Specs []*AttachSpec
}

// PhysField is one projection output; its expression is crossing-free.
type PhysField struct {
	Name string
	Slot lir.SlotID
	Expr bound.Expr
}

// ProjectExec assembles output rows from pure expressions — any crossing a
// field once contained now arrives as an attach slot from below.
type ProjectExec struct {
	Input  PhysNode
	Fields []PhysField
}

// SortExec is the blocking sort: stable, Value.Compare semantics (NULLs
// first ascending, last descending via negation).
type SortExec struct {
	Input PhysNode
	Terms []bound.OrderTerm
}

// SliceExec skips Offset rows and stops pulling after Limit accepted rows.
type SliceExec struct {
	Input  PhysNode
	Offset int
	Limit  *int
}

// RefExec is one occurrence of a binding: it re-exposes the committed
// canonical frames under the occurrence's fresh slots. Canon aligns with
// Out.Fields.
type RefExec struct {
	Binding string
	Out     lir.RowType
	Canon   []lir.SlotID
}

// RecursiveRefExec is one occurrence of a recursive binding's frontier: it
// re-exposes the current working table's canonical frames under the
// occurrence's fresh slots, exactly as RefExec does for a committed binding.
type RecursiveRefExec struct {
	Binding string
	Out     lir.RowType
	Canon   []lir.SlotID
}

// DistinctExec removes duplicate complete rows, blocking: it drains its input,
// admits each canonical row once, and emits the survivors. Order is not
// preserved. Out carries the output fields the row identity is taken over.
type DistinctExec struct {
	Input PhysNode
	Out   lir.RowType
}

// NestedLoopJoinExec joins by materialising the right input and probing it
// per left row. Inner and left only; ROut is the right side's row type, for
// NULL-padding unmatched left rows.
type NestedLoopJoinExec struct {
	L, R PhysNode
	Kind lir.JoinKind
	On   bound.Expr
	ROut lir.RowType
}

// AggregateExec folds its input: one row per distinct group, or exactly one
// row with no groups.
type AggregateExec struct {
	Input  PhysNode
	Groups []bound.GroupTerm
	Terms  []bound.AggTerm
}

func (*PKGetExec) phys()          {}
func (*TableScanExec) phys()      {}
func (*RowsExec) phys()           {}
func (*IndexRangeScanExec) phys() {}
func (*FilterExec) phys()         {}
func (*AttachExec) phys()         {}
func (*ProjectExec) phys()        {}
func (*SortExec) phys()           {}
func (*SliceExec) phys()          {}
func (*NestedLoopJoinExec) phys() {}
func (*RefExec) phys()            {}
func (*RecursiveRefExec) phys()   {}
func (*DistinctExec) phys()       {}
func (*AggregateExec) phys()      {}
