// Package reject marks errors by who caused them, so transport layers map
// them to problem responses by type instead of by string prefix. It imports
// nothing and sits below every layer.
//
// Two classes exist. Input errors are the caller's fault at request time —
// an unknown table, a malformed value, a constraint the write would violate
// — and are always safe to relay. Runtime errors are data-dependent
// failures of a valid request — division by zero, a cardinality assertion
// that the data refutes. Anything unmarked is internal: transports must
// treat it as a 500 and hide the detail.
//
// The message conventions ("planner:", "exec:" prefixes) are unchanged —
// they are useful diagnostics. They are just no longer the classifier.
package reject

import (
	"errors"
	"fmt"
)

type inputError struct{ err error }

func (e inputError) Error() string { return e.err.Error() }
func (e inputError) Unwrap() error { return e.err }

type runtimeError struct{ err error }

func (e runtimeError) Error() string { return e.err.Error() }
func (e runtimeError) Unwrap() error { return e.err }

// Inputf builds a caller-fault error.
func Inputf(format string, args ...any) error {
	return inputError{err: fmt.Errorf(format, args...)}
}

// Input marks an existing error as caller-fault.
func Input(err error) error {
	if err == nil {
		return nil
	}
	return inputError{err: err}
}

// Runtimef builds a data-dependent runtime failure.
func Runtimef(format string, args ...any) error {
	return runtimeError{err: fmt.Errorf(format, args...)}
}

// IsInput reports whether err (anywhere in its chain) is caller-fault.
func IsInput(err error) bool {
	var t inputError
	return errors.As(err, &t)
}

// IsRuntime reports whether err (anywhere in its chain) is a data-dependent
// runtime failure.
func IsRuntime(err error) bool {
	var t runtimeError
	return errors.As(err, &t)
}
