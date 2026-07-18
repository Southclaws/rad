package explain_test

import (
	"testing"

	lir "github.com/Southclaws/rad/rad/engine/03_lir"
	"github.com/Southclaws/rad/rad/engine/03_lir/bound"
	pt "github.com/Southclaws/rad/rad/engine/04_planner/plannertest"
)

var (
	bcol    = pt.Column
	blit    = pt.Literal
	beq     = pt.Equal
	band    = pt.And
	bscan   = pt.Scan
	bfilter = pt.Filter
)

func bind(t *testing.T, q lir.Query) *bound.Query { return pt.Bind(t, q) }
