// Package program compiles ordered relational statements into physical plans
// while preserving the shared slot space and preceding statement bindings.
package program

import (
	"context"

	lir "github.com/Southclaws/rad/rad/engine/03_lir"
	binder "github.com/Southclaws/rad/rad/engine/04_planner/bind"
	"github.com/Southclaws/rad/rad/engine/04_planner/physical"
)

type Statement struct {
	Name     string
	Rel      lir.Query
	Mutation bool
	Table    string
}

type BoundStatement struct {
	Name       string
	Plan       *physical.PhysPlan
	ResultOut  lir.RowType
	ResultCard lir.RootCard
}

type Binder struct {
	inner *binder.ProgramBinder
}

func NewBinder(ctx context.Context, cat binder.Catalog, names []string) (*Binder, error) {
	inner, err := binder.NewProgramBinder(ctx, cat, names)
	if err != nil {
		return nil, err
	}
	return &Binder{inner: inner}, nil
}

func (b *Binder) Bind(statement Statement) (BoundStatement, error) {
	result, err := b.inner.Bind(binder.ProgramStmt{
		Name: statement.Name, Rel: statement.Rel,
		Mutation: statement.Mutation, Table: statement.Table,
	})
	if err != nil {
		return BoundStatement{}, err
	}
	return BoundStatement{
		Name: result.Name, Plan: result.Plan,
		ResultOut: result.ResultOut, ResultCard: result.ResultCard,
	}, nil
}

func Bind(ctx context.Context, cat binder.Catalog, statements []Statement) ([]BoundStatement, error) {
	names := make([]string, len(statements))
	for i, statement := range statements {
		names[i] = statement.Name
	}
	b, err := NewBinder(ctx, cat, names)
	if err != nil {
		return nil, err
	}
	result := make([]BoundStatement, 0, len(statements))
	for _, statement := range statements {
		bound, err := b.Bind(statement)
		if err != nil {
			return nil, err
		}
		result = append(result, bound)
	}
	return result, nil
}
