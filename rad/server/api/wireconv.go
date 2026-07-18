package api

import (
	"fmt"

	catalog "github.com/Southclaws/rad/rad/engine/02_catalog"
)

// wireErr is a client-caused conversion error (HTTP 400).
type wireErr struct{ msg string }

func (e wireErr) Error() string { return e.msg }

func wireErrf(format string, args ...any) error {
	return wireErr{fmt.Sprintf(format, args...)}
}

// wireConv resolves tables for cell coercion.
type wireConv struct {
	cat *catalog.Catalog
}
