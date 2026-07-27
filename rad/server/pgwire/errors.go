package pgwire

import (
	"strings"

	"github.com/jeroenrinzema/psql-wire/codes"
	psqlerr "github.com/jeroenrinzema/psql-wire/errors"

	"github.com/Southclaws/rad/rad/engine/reject"
	"github.com/Southclaws/rad/rad/sql"
)

// isConflict recognizes a lost serializable race. The reject classification
// covers engine-level conflicts; the text fallback catches raw KV-layer
// conflicts that surface without a reason attached.
func isConflict(err error) bool {
	if reject.IsConflict(err) {
		return true
	}
	return err != nil && strings.Contains(err.Error(), "transaction conflict")
}

// sqlstate attaches the closest SQLSTATE to an engine or compiler error so
// drivers and ORMs classify it the way they would a real Postgres error
// (ent matches unique violations by code 23505, retry loops watch 40001).
func sqlstate(err error) error {
	if err == nil {
		return nil
	}
	if sql.IsUnsupported(err) {
		return psqlerr.WithCode(err, codes.FeatureNotSupported)
	}
	reason, _ := reject.ReasonOf(err)
	if reason == reject.ReasonTransitionBackpressure || reason == reject.ReasonTransitionFinalizing {
		return psqlerr.WithCode(err, codes.LockNotAvailable)
	}
	if isConflict(err) {
		return psqlerr.WithCode(err, codes.SerializationFailure)
	}
	// Primary-key collisions surface as a distinct engine message, not the
	// constraint_violation reason; drivers still expect unique_violation.
	if strings.Contains(err.Error(), "duplicate primary key") ||
		strings.Contains(err.Error(), "two rows with the same primary key") {
		return psqlerr.WithCode(err, codes.UniqueViolation)
	}
	var code codes.Code
	switch reason {
	case reject.ReasonConstraintViolation:
		code = codes.UniqueViolation
	case reject.ReasonSerializableConflict:
		code = codes.SerializationFailure
	case reject.ReasonUnknownTable:
		code = codes.UndefinedTable
	case reject.ReasonUnknownColumn:
		code = codes.UndefinedColumn
	case reject.ReasonDivisionByZero:
		code = codes.DivisionByZero
	case reject.ReasonTypeMismatch:
		code = codes.DatatypeMismatch
	case reject.ReasonCardinality:
		code = codes.CardinalityViolation
	case reject.ReasonSchemaManaged:
		code = codes.FeatureNotSupported
	case reject.ReasonNotFound:
		code = codes.UndefinedObject
	default:
		if reject.IsInput(err) {
			code = codes.SyntaxErrorOrAccessRuleViolation
		} else {
			code = codes.Internal
		}
	}
	return psqlerr.WithCode(err, code)
}
