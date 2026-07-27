package model

import "slices"

// CatalogDependencies is the immutable compatibility contract carried by a
// bound physical plan. Names are diagnostic only; physical IDs and expected
// fence generations determine identity and admission.
//
// This is internal execution metadata. It is neither persisted nor exposed
// through PIR or LIR, so it deliberately has no serialization tags.
type CatalogDependencies struct {
	TableExistence []TableExistenceDependency
	ColumnValues   []ColumnValueDependency
	IndexAccess    []IndexAccessDependency
	WriteProtocols []WriteProtocolDependency
}

// TableExistenceDependency pins the existence fence of one physical table.
type TableExistenceDependency struct {
	TableID    string
	TableName  string
	Generation uint64
}

// ColumnValueDependency pins one physical column's decoding and missing-value
// semantics.
type ColumnValueDependency struct {
	TableID    string
	TableName  string
	ColumnID   string
	ColumnName string
	Generation uint64
}

// IndexAccessDependency pins the lifetime and representation of one physical
// index selected by a plan.
type IndexAccessDependency struct {
	TableID    string
	TableName  string
	IndexID    string
	IndexName  string
	Generation uint64
}

// WriteProtocolDependency pins the complete mutation obligations for one
// table.
type WriteProtocolDependency struct {
	TableID    string
	TableName  string
	Generation uint64
}

// Empty reports whether the manifest admits no catalog compatibility fences.
func (d CatalogDependencies) Empty() bool {
	return len(d.TableExistence) == 0 &&
		len(d.ColumnValues) == 0 &&
		len(d.IndexAccess) == 0 &&
		len(d.WriteProtocols) == 0
}

// Clone returns an independently owned copy of the dependency manifest.
func (d CatalogDependencies) Clone() CatalogDependencies {
	return CatalogDependencies{
		TableExistence: slices.Clone(d.TableExistence),
		ColumnValues:   slices.Clone(d.ColumnValues),
		IndexAccess:    slices.Clone(d.IndexAccess),
		WriteProtocols: slices.Clone(d.WriteProtocols),
	}
}

// AddTableRead records the value semantics needed to decode columns from one
// live physical table. An empty columns slice is valid for row-counting plans:
// they depend on the table and its rows, but decode no cells.
func (d *CatalogDependencies) AddTableRead(table Table, columns []Column) {
	d.addTableExistence(TableExistenceDependency{
		TableID: table.ID, TableName: table.Name, Generation: table.ExistenceGeneration,
	})
	for _, column := range columns {
		d.addColumnValue(ColumnValueDependency{
			TableID: table.ID, TableName: table.Name,
			ColumnID: column.ID, ColumnName: column.Name, Generation: column.ValueGeneration,
		})
	}
}

// AddIndexRead extends a table read with the physical index access path whose
// lifetime the plan relies upon.
func (d *CatalogDependencies) AddIndexRead(table Table, index Index, columns []Column) {
	d.AddTableRead(table, columns)
	d.addIndexAccess(IndexAccessDependency{
		TableID: table.ID, TableName: table.Name,
		IndexID: index.ID, IndexName: index.Name, Generation: index.AccessGeneration,
	})
}

// AddTableWrite records the complete current row definition and immutable
// write protocol a mutation must honor. Ready-index maintenance and active
// schema-transition obligations are members of that protocol.
func (d *CatalogDependencies) AddTableWrite(table Table) {
	d.AddTableRead(table, table.Columns)
	d.addWriteProtocol(WriteProtocolDependency{
		TableID: table.ID, TableName: table.Name, Generation: table.WriteProtocolGeneration,
	})
}

// Merge adds every dependency from other, deduplicating identical fences.
func (d *CatalogDependencies) Merge(other CatalogDependencies) {
	for _, dependency := range other.TableExistence {
		d.addTableExistence(dependency)
	}
	for _, dependency := range other.ColumnValues {
		d.addColumnValue(dependency)
	}
	for _, dependency := range other.IndexAccess {
		d.addIndexAccess(dependency)
	}
	for _, dependency := range other.WriteProtocols {
		d.addWriteProtocol(dependency)
	}
}

func (d *CatalogDependencies) addTableExistence(dependency TableExistenceDependency) {
	if slices.ContainsFunc(d.TableExistence, func(existing TableExistenceDependency) bool {
		return existing.TableID == dependency.TableID && existing.Generation == dependency.Generation
	}) {
		return
	}
	d.TableExistence = append(d.TableExistence, dependency)
}

func (d *CatalogDependencies) addColumnValue(dependency ColumnValueDependency) {
	if slices.ContainsFunc(d.ColumnValues, func(existing ColumnValueDependency) bool {
		return existing.TableID == dependency.TableID &&
			existing.ColumnID == dependency.ColumnID &&
			existing.Generation == dependency.Generation
	}) {
		return
	}
	d.ColumnValues = append(d.ColumnValues, dependency)
}

func (d *CatalogDependencies) addIndexAccess(dependency IndexAccessDependency) {
	if slices.ContainsFunc(d.IndexAccess, func(existing IndexAccessDependency) bool {
		return existing.TableID == dependency.TableID &&
			existing.IndexID == dependency.IndexID &&
			existing.Generation == dependency.Generation
	}) {
		return
	}
	d.IndexAccess = append(d.IndexAccess, dependency)
}

func (d *CatalogDependencies) addWriteProtocol(dependency WriteProtocolDependency) {
	if slices.ContainsFunc(d.WriteProtocols, func(existing WriteProtocolDependency) bool {
		return existing.TableID == dependency.TableID && existing.Generation == dependency.Generation
	}) {
		return
	}
	d.WriteProtocols = append(d.WriteProtocols, dependency)
}
