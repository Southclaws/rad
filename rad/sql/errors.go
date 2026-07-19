package sql

import (
	"errors"
	"fmt"
)

// UnsupportedError marks SQL that parses but falls outside the compilable
// subset — either LIR has no target construct (LIKE, CASE, set operations,
// jsonb operators) or the frontend deliberately rejects the operation. The
// wire layer maps it to SQLSTATE 0A000 (feature_not_supported), keeping it
// distinguishable from genuinely malformed queries.
type UnsupportedError struct {
	Feature string
}

func (e *UnsupportedError) Error() string {
	return "unsupported: " + e.Feature
}

func unsupportedf(format string, args ...any) error {
	return &UnsupportedError{Feature: fmt.Sprintf(format, args...)}
}

// IsUnsupported reports whether err is an UnsupportedError.
func IsUnsupported(err error) bool {
	var u *UnsupportedError
	return errors.As(err, &u)
}
