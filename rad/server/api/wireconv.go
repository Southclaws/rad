package api

import (
	"fmt"
)

// wireErr is a client-caused conversion error (HTTP 400).
type wireErr struct{ msg string }

func (e wireErr) Error() string { return e.msg }

func wireErrf(format string, args ...any) error {
	return wireErr{fmt.Sprintf(format, args...)}
}
