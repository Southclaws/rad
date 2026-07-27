package api

import (
	"github.com/Southclaws/rad/rad/api/oas"
	"github.com/Southclaws/rad/rad/protocol"
)

// ProblemToOAS converts a Problem into the generated type.
func ProblemToOAS(p protocol.Problem) oas.Problem {
	o := oas.Problem{
		Type:   p.Type,
		Title:  p.Title,
		Status: p.Status,
		Code:   oas.ProblemCode(p.Code),
		Reason: p.Reason,
	}
	if p.Detail != "" {
		o.Detail = oas.NewOptString(p.Detail)
	}
	return o
}

// ProblemFromOAS converts a generated Problem back into the wire type.
func ProblemFromOAS(o oas.Problem) protocol.Problem {
	return protocol.Problem{
		Type:   o.Type,
		Title:  o.Title,
		Status: o.Status,
		Detail: o.Detail.Or(""),
		Code:   string(o.Code),
		Reason: o.Reason,
	}
}
