package frontend_test

import (
	"context"
	"fmt"
	"sort"

	"rad/rad/01_kv/kvmem"
	catalog "rad/rad/02_catalog"
	lir "rad/rad/03_lir"
	frontend "rad/rad/06_frontend"
)

// The complete lifecycle through the public API: initialize a database,
// define related tables, write transactionally, and run a joined query.
// Swap kvmem.New() for kvslate.Open(...) and the same code runs on SlateDB.
func Example() {
	ctx := context.Background()

	db, err := frontend.Init(ctx, kvmem.New(), "public")
	if err != nil {
		panic(err)
	}

	if _, err := db.CreateTable(ctx, catalog.TableDef{
		Name: "authors",
		Columns: []catalog.ColumnDef{
			{Name: "id", Type: catalog.TypeInt64},
			{Name: "name", Type: catalog.TypeText},
		},
		PrimaryKey: []string{"id"},
	}); err != nil {
		panic(err)
	}
	if _, err := db.CreateTable(ctx, catalog.TableDef{
		Name: "books",
		Columns: []catalog.ColumnDef{
			{Name: "id", Type: catalog.TypeInt64},
			{Name: "author_id", Type: catalog.TypeInt64},
			{Name: "title", Type: catalog.TypeText},
		},
		PrimaryKey:  []string{"id"},
		Indexes:     []catalog.IndexDef{{Name: "books_author_idx", Columns: []string{"author_id"}}},
		ForeignKeys: []catalog.ForeignKeyDef{{Name: "books_author_fk", Columns: []string{"author_id"}, RefTable: "authors", RefColumns: []string{"id"}}},
	}); err != nil {
		panic(err)
	}

	// An author and her books commit atomically.
	err = db.Txn(ctx, func(tx *frontend.Tx) error {
		if err := tx.Insert(ctx, "authors", lir.Row{"id": lir.Int64(1), "name": lir.Text("Ursula K. Le Guin")}); err != nil {
			return err
		}
		if err := tx.Insert(ctx, "books", lir.Row{"id": lir.Int64(10), "author_id": lir.Int64(1), "title": lir.Text("A Wizard of Earthsea")}); err != nil {
			return err
		}
		return tx.Insert(ctx, "books", lir.Row{"id": lir.Int64(11), "author_id": lir.Int64(1), "title": lir.Text("The Dispossessed")})
	})
	if err != nil {
		panic(err)
	}

	// "Titles by author 1", via the secondary index, projected and joined.
	rows, err := db.Query(ctx, lir.Query{Root: lir.Project{
		Input: lir.Join{
			Left:  lir.Scan{Table: "authors", Alias: "a"},
			Right: lir.IndexScan{Table: "books", Alias: "b", Index: "books_author_idx", Prefix: lir.Row{"author_id": lir.Int64(1)}},
			Type:  lir.InnerJoin,
			On: lir.Eq{
				Left:  lir.ColRef{Alias: "a", Column: "id"},
				Right: lir.ColRef{Alias: "b", Column: "author_id"},
			},
		},
		Columns: []lir.Projection{
			{Expr: lir.ColRef{Alias: "a", Column: "name"}, As: "author"},
			{Expr: lir.ColRef{Alias: "b", Column: "title"}, As: "title"},
		},
	}})
	if err != nil {
		panic(err)
	}

	printRows(rows)
	// Output:
	// author="Ursula K. Le Guin"  title="A Wizard of Earthsea"
	// author="Ursula K. Le Guin"  title="The Dispossessed"
}

// printRows renders rows with columns in sorted order (Go maps iterate
// randomly).
func printRows(rows []lir.Row) {
	for _, row := range rows {
		cols := make([]string, 0, len(row))
		for name := range row {
			cols = append(cols, name)
		}
		sort.Strings(cols)
		for i, name := range cols {
			if i > 0 {
				fmt.Print("  ")
			}
			fmt.Printf("%s=%s", name, row[name])
		}
		fmt.Println()
	}
}
