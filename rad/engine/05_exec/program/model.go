// Package program defines the physical intermediate program executed by the
// engine: ordered relational and catalog statements plus execution policy.
package program

import (
	"github.com/Southclaws/rad/rad/engine/02_catalog/model"
	lir "github.com/Southclaws/rad/rad/engine/03_lir"
	"github.com/Southclaws/rad/rad/engine/04_planner/explain"
)

type Kind string

const (
	Query  Kind = "query"
	Create Kind = "create"
	Update Kind = "update"
	Delete Kind = "delete"

	CreateTable  Kind = "create_table"
	RenameTable  Kind = "rename_table"
	DeleteTable  Kind = "delete_table"
	CreateColumn Kind = "create_column"
	RenameColumn Kind = "rename_column"
	DeleteColumn Kind = "delete_column"
	CreateIndex  Kind = "create_index"
	DeleteIndex  Kind = "delete_index"
)

type Statement struct {
	Name      string
	Kind      Kind
	Table     string
	Rel       lir.Query
	TableID   model.SchemaID
	ColumnID  model.SchemaID
	To        string
	TableDef  model.TableDef
	Column    model.ColumnDef
	Index     model.IndexDef
	IndexName string
}

type Program struct {
	Statements []Statement
	Result     string
}

type CatalogPolicy uint8

const (
	CatalogForbidden CatalogPolicy = iota
	CatalogRevisionPerStatement
	CatalogRevisionPerProgram
)

type Options struct {
	DryRun          bool
	CollectPlan     bool
	Catalog         CatalogPolicy
	ExpectedCatalog *model.Revision
}

type StatementResult struct {
	Name     string
	Affected int
}

type StatementPlan struct {
	Name string
	View *explain.PlanView
}

type Result struct {
	Result     lir.Datum
	Statements []StatementResult
	Plans      []StatementPlan
}

func (k Kind) Catalog() bool {
	switch k {
	case CreateTable, RenameTable, DeleteTable, CreateColumn, RenameColumn, DeleteColumn, CreateIndex, DeleteIndex:
		return true
	default:
		return false
	}
}

func (k Kind) Valid() bool {
	switch k {
	case Query, Create, Update, Delete:
		return true
	default:
		return k.Catalog()
	}
}
