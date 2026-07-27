// Package reject marks errors with a stable, machine-readable reason, so
// transport layers map them to problem responses by classification instead
// of by string prefix. It imports nothing and sits below every layer.
//
// Two levels, following the error-propagation design:
//
//   - Class is the coarse category that maps to an HTTP status and to
//     retryability: invalid (the request can never succeed as-is),
//     execution_failed (a valid request failed on the data it met),
//     conflict (a lost serializable race — retry may win), not_found, and
//     internal (unmarked errors; transports hide the detail behind a 500).
//   - Reason is the fine-grained, stable member within a class
//     (unknown_table, division_by_zero, mutation_target_not_found, …). Every
//     reason belongs to exactly one class, so a reason implies its class.
//
// A site names a reason with Fail; the transport reads it back with
// ReasonOf. Inputf/Runtimef remain for sites not yet given a specific
// reason — they carry the class catch-all — so classification never
// regresses to "internal" during the migration.
//
// The message conventions ("planner:", "exec:" prefixes) are unchanged: they
// are useful diagnostics, not the classifier.
package reject

import (
	"errors"
	"fmt"
)

// Class is the coarse error category. The five values map one-to-one to the
// wire problem codes and to HTTP status.
type Class string

const (
	ClassInvalid         Class = "invalid"
	ClassExecutionFailed Class = "execution_failed"
	ClassConflict        Class = "conflict"
	ClassNotFound        Class = "not_found"
	ClassInternal        Class = "internal"
)

// Reason is the stable, fine-grained error identity within a class. New
// reasons accumulate freely; the set of classes almost never changes.
type Reason string

const (
	// invalid — the request cannot succeed without changing the request or
	// the data it references.
	ReasonInvalid             Reason = "invalid" // class catch-all (Inputf)
	ReasonSchemaViolation     Reason = "schema_violation"
	ReasonUnknownTable        Reason = "unknown_table"
	ReasonUnknownColumn       Reason = "unknown_column"
	ReasonUnknownScope        Reason = "unknown_scope"
	ReasonUnknownBinding      Reason = "unknown_binding"
	ReasonDuplicateScope      Reason = "duplicate_scope"
	ReasonTypeMismatch        Reason = "type_mismatch"
	ReasonScalarArity         Reason = "scalar_arity"
	ReasonNondeterministic    Reason = "nondeterministic_order"
	ReasonDependentJoin       Reason = "dependent_join"
	ReasonProjectionCollision Reason = "projection_collision"
	ReasonBindingCycle        Reason = "binding_cycle"
	ReasonBindingCollision    Reason = "binding_output_collision"
	ReasonConstraintViolation Reason = "constraint_violation"
	ReasonMutationAmbiguous   Reason = "mutation_target_ambiguous"
	ReasonMutationShape       Reason = "mutation_shape"
	ReasonSchemaManaged       Reason = "schema_managed"
	ReasonDataLossAcceptance  Reason = "schema_data_loss_acceptance_required"

	// execution_failed — a valid request failed on the data it met.
	ReasonExecutionFailed  Reason = "execution_failed" // class catch-all (Runtimef)
	ReasonDivisionByZero   Reason = "division_by_zero"
	ReasonCardinality      Reason = "cardinality_violation"
	ReasonMutationNotFound Reason = "mutation_target_not_found"
	ReasonRecursionLimit   Reason = "recursion_limit"

	// conflict — a lost serializable race; retrying the whole unit may win.
	ReasonSerializableConflict   Reason = "serializable_conflict"
	ReasonTransitionBackpressure Reason = "schema_transition_backpressure"
	ReasonTransitionFinalizing   Reason = "schema_transition_finalizing"

	// not_found — the addressed resource does not exist.
	ReasonNotFound Reason = "not_found"

	// internal — an unexpected failure; the detail is hidden from the caller.
	ReasonInternal       Reason = "internal"
	ReasonCatalogCorrupt Reason = "catalog_corrupt"
	ReasonCatalogDrift   Reason = "catalog_schema_drift"
)

// reasonClass maps every reason to its class. A reason missing here is a
// programming error and classifies as internal (fail closed, hide detail).
var reasonClass = map[Reason]Class{
	ReasonInvalid:             ClassInvalid,
	ReasonSchemaViolation:     ClassInvalid,
	ReasonUnknownTable:        ClassInvalid,
	ReasonUnknownColumn:       ClassInvalid,
	ReasonUnknownScope:        ClassInvalid,
	ReasonUnknownBinding:      ClassInvalid,
	ReasonDuplicateScope:      ClassInvalid,
	ReasonTypeMismatch:        ClassInvalid,
	ReasonScalarArity:         ClassInvalid,
	ReasonNondeterministic:    ClassInvalid,
	ReasonDependentJoin:       ClassInvalid,
	ReasonProjectionCollision: ClassInvalid,
	ReasonBindingCycle:        ClassInvalid,
	ReasonBindingCollision:    ClassInvalid,
	ReasonConstraintViolation: ClassInvalid,
	ReasonMutationAmbiguous:   ClassInvalid,
	ReasonMutationShape:       ClassInvalid,
	ReasonSchemaManaged:       ClassInvalid,
	ReasonDataLossAcceptance:  ClassInvalid,

	ReasonExecutionFailed:  ClassExecutionFailed,
	ReasonDivisionByZero:   ClassExecutionFailed,
	ReasonCardinality:      ClassExecutionFailed,
	ReasonMutationNotFound: ClassExecutionFailed,
	ReasonRecursionLimit:   ClassExecutionFailed,

	ReasonSerializableConflict:   ClassConflict,
	ReasonTransitionBackpressure: ClassConflict,
	ReasonTransitionFinalizing:   ClassConflict,
	ReasonNotFound:               ClassNotFound,
	ReasonInternal:               ClassInternal,
	ReasonCatalogCorrupt:         ClassInternal,
	ReasonCatalogDrift:           ClassInternal,
}

// Class returns the reason's class, or ClassInternal for an unregistered
// reason.
func (r Reason) Class() Class {
	if c, ok := reasonClass[r]; ok {
		return c
	}
	return ClassInternal
}

// radError carries a reason alongside a wrapped cause.
type radError struct {
	reason Reason
	err    error
}

func (e radError) Error() string { return e.err.Error() }
func (e radError) Unwrap() error { return e.err }

// Fail builds an error with a specific reason and a formatted message.
func Fail(r Reason, format string, args ...any) error {
	return radError{reason: r, err: fmt.Errorf(format, args...)}
}

// Mark tags an existing error with a reason, preserving its chain.
func Mark(r Reason, err error) error {
	if err == nil {
		return nil
	}
	return radError{reason: r, err: err}
}

// ReasonOf returns the reason of the first marked error in the chain.
func ReasonOf(err error) (Reason, bool) {
	var e radError
	if errors.As(err, &e) {
		return e.reason, true
	}
	return "", false
}

// Inputf builds a caller-fault error with the invalid class catch-all reason.
// Prefer Fail with a specific reason at new or migrated sites.
func Inputf(format string, args ...any) error {
	return Fail(ReasonInvalid, format, args...)
}

// Input marks an existing error as caller-fault (invalid).
func Input(err error) error {
	return Mark(ReasonInvalid, err)
}

// Runtimef builds a data-dependent runtime failure with the execution_failed
// catch-all reason.
func Runtimef(format string, args ...any) error {
	return Fail(ReasonExecutionFailed, format, args...)
}

// IsInput reports whether err is caller-fault (its reason is class invalid).
func IsInput(err error) bool {
	r, ok := ReasonOf(err)
	return ok && r.Class() == ClassInvalid
}

// IsRuntime reports whether err is a data-dependent runtime failure
// (execution_failed).
func IsRuntime(err error) bool {
	r, ok := ReasonOf(err)
	return ok && r.Class() == ClassExecutionFailed
}

// IsConflict reports whether err names a retryable conflict reason.
func IsConflict(err error) bool {
	r, ok := ReasonOf(err)
	return ok && r.Class() == ClassConflict
}

// IsNotFound reports whether err addresses a resource that does not exist.
func IsNotFound(err error) bool {
	r, ok := ReasonOf(err)
	return ok && r.Class() == ClassNotFound
}
