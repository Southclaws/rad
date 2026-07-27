package api

import (
	"encoding/json"
	"strconv"

	"github.com/Southclaws/rad/rad/protocol/lirwire"
	"github.com/Southclaws/rad/rad/protocol/pirwire"
)

// These builders keep integration tests on the generated LIR/PIR wire types.
func relBytes(query lirwire.Query) pirwire.Relation {
	encoded, _ := json.Marshal(query)
	return encoded
}

func mustValue(value any) lirwire.Cell {
	if value == nil {
		return nil
	}
	switch typed := value.(type) {
	case string:
		return &typed
	case int:
		encoded := strconv.FormatInt(int64(typed), 10)
		return &encoded
	case int64:
		encoded := strconv.FormatInt(typed, 10)
		return &encoded
	case float64:
		encoded := strconv.FormatFloat(typed, 'g', -1, 64)
		return &encoded
	case bool:
		encoded := strconv.FormatBool(typed)
		return &encoded
	}
	return nil
}

func ptrBool(value bool) *bool                 { return &value }
func ptrExpr(value lirwire.Expr) *lirwire.Expr { return &value }
