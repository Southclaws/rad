# Executor glossary

This glossary defines the relational and execution terms used in this package
and in the plans it consumes. It is a vocabulary guide, not a description of
the package layout or an execution procedure. The
[engine architecture guide](../README.md) defines catalog MVCC, schema jobs,
write protocols, retention, and reclamation.

## Values and relations

**Value**  
A typed scalar such as text, an integer, a float, or a boolean. A value may be
`NULL`.

**Datum**  
A result value. A datum can be `NULL`, a scalar value, an object, or an array.
Unlike a scalar value, it can therefore represent the nested result of a
subquery.

**Row**  
One item in a relation. Its attributes have names and types. A table row is a
stored row; a query row may be assembled from several tables, expressions, or
nested datums.

**Relation**  
A possibly empty collection of rows with a fixed row type. Relations have bag
semantics: unless an operation such as `distinct` removes them, duplicate rows
remain significant.

**Expression**  
A computation that produces exactly one typed datum in the current scope. An
expression can read fields from a frame, but it can consume a relation only
through an explicit crossing.

**Predicate**  
An expression used as a condition by an operation such as a filter or join.
Its logical result can be `TRUE`, `FALSE`, or `UNKNOWN`.

**Bag (multiset)**  
A collection in which the same row may occur more than once. A set, by
contrast, contains each distinct row at most once.

**Relational closure**  
The property that an operation on relations produces another relation. This
allows scans, filters, joins, projections, aggregates, and similar operations
to be composed without introducing a different intermediate kind.

**Row type**  
The ordered shape of a relation's rows: each field's name, slot, static type,
and nullability. It describes the row independently of any particular row's
values.

**Cardinality**  
The number of rows a relation may produce. Static cardinality is usually
expressed as lower and upper bounds, such as `0..1`, `1..1`, or `0..many`.
Root cardinality also states how a query result is shaped, such as one object,
one scalar, or an array of objects.

**`NULL` and `UNKNOWN`**  
`NULL` represents an absent scalar value. Comparisons involving `NULL` can
produce the logical result `UNKNOWN`; a filter keeps only rows for which its
predicate is `TRUE`.

## Names and execution state

**Scope**  
A query-level name for one occurrence of a relation. Column references use a
scope to distinguish otherwise identical column names, especially across
joins and correlated subqueries.

**Slot**  
A dense identifier assigned to a field after names have been bound. Execution
uses slots instead of repeatedly resolving scope and column names. Two
occurrences of the same relation receive different slots even when their field
names match.

**Frame**  
The executor's environment for one in-flight row, represented as a mapping
from slots to datums. A frame is not necessarily a stored table row: it can
contain fields assembled from several relations, computed values, attached
subquery results, and values inherited from an outer relation.

**Outer environment**  
The slots visible from an enclosing relation. A correlated subquery reads this
environment when it refers to fields produced outside itself.

**Binding**  
A name given to a relational value within a query or program. A binding is
referenced as a relation; it is not a scalar variable and does not imply that
the value is stored in a table.

**Reference (ref)**  
One occurrence of a binding. Each reference gives the binding's fields fresh
slots, just as separate scans of one table create separate query scopes.

**Commitment**  
The relational value denoted by a binding and observed by all of its
references. This use of “commit” is about a binding's meaning, not a storage
transaction commit.

**Materialisation (materialization)**  
Evaluating a relation and retaining its frames so they can be consumed as a
completed value or read more than once. Materialisation is an execution
property; it does not mean that rows are persisted to storage.

**Replay**  
Streaming a binding's plan directly at its single reference rather than
retaining a separate copy of all its frames. The evaluation still constitutes
that binding's commitment.

## Planning and operators

**LIR (logical intermediate representation)**  
The relation and expression graph that states what a query means without
naming storage access methods.

**PIR (program intermediate representation)**  
An ordered program of query and mutation statements that share one
transaction. A statement can expose its result as a binding to later
statements.

**Unbound / bound**  
Unbound relations and expressions refer to tables, scopes, and columns by
name. Binding resolves those names against the catalog, checks types and
cardinality rules, and replaces field references with slots.

**Catalog**  
The schema information available to binding and planning: tables, columns,
types, keys, indexes, and constraints. The catalog describes stored data; it
does not contain the table rows themselves.

**Physical plan**  
The executable operator tree selected for a bound relation. It records choices
such as a table scan, primary-key lookup, or index range scan.

**Plan-choice sensitivity**  
The property that separate legal evaluations could choose different members
or orderings where the query does not determine a unique answer. A sensitive
binding must have one commitment shared by its references.

**Operator**  
In the executor, a pull-based iterator over frames. Its caller asks for the
next frame until the input is exhausted or an error occurs. In relational
discussion, “operator” can also refer more generally to an operation such as a
filter or join.

**Streaming operator**  
An operator that can produce output while it consumes its input. Scans,
filters, projections, and slices are typical examples.

**Blocking operator**  
An operator that must consume some or all of its input before producing the
relevant output. Sorting and whole-input aggregation are typical examples.

**Scan**  
A relation that reads rows from a table. A scan introduces a scope; its
physical access path determines how candidate rows are found.

**Access path**  
The storage method used to obtain candidate rows, such as scanning the table,
looking up a primary key, or scanning an index range. An access path is a
physical choice and must not change the relation's meaning.

**Residual predicate**  
The full logical condition checked after an access path has selected candidate
rows. An index may use part of a predicate to narrow the scan, but the residual
check preserves the complete filtering semantics.

**Projection**  
An operation that defines a new row shape by selecting existing fields and
evaluating expressions. Projection is also how nested query results become
fields of an output row.

**Join**  
An operation that combines rows from two relations according to a condition.
An inner join emits matching combinations; a left join also emits unmatched
left rows with `NULL` values for the right side.

**Aggregate / fold**  
An operation that reduces input rows into values such as `count`, `sum`, or
`max`. A grouped aggregate emits one row per group; a global aggregate emits
one row for the whole input, including an empty input.

**Distinct**  
An operation that removes duplicate complete rows using a stable, typed row
identity. Predicate equality and distinct-row identity differ around `NULL`:
two `NULL` fields occupy the same position in an otherwise identical row.

## Nested relations

**Crossing (cardinality crossing)**  
The explicit conversion of a relation, which may contain many rows, into one
datum that an expression can use. A relation cannot be used as an expression
without stating how its rows cross this boundary.

| Crossing | Result                                                               |
| -------- | -------------------------------------------------------------------- |
| `exists` | A boolean stating whether the relation has any rows.                 |
| `first`  | The selected row as an object, or `NULL` when empty.                 |
| `scalar` | The only field of an at-most-one-row relation, or `NULL` when empty. |
| `array`  | Every row as an array of objects; empty input yields an empty array. |

**Correlation**  
A dependency from a nested relation on slots produced by an enclosing
relation. An uncorrelated relation has no such dependency.

**Key correlation**  
A correlation whose outer dependencies are equality keys for an inner scan.
Outer frames with the same key can share the same nested result.

**General correlation**  
A correlation that depends on outer values in a way that cannot be represented
solely as inner scan keys. Its result may need to be evaluated for each outer
frame.

**Attach**  
The physical operation paired with a crossing. It evaluates the crossing's
relation, converts the produced frames to the requested datum, and places that
datum in a fresh slot on the outer frame. Ordinary expression evaluation can
then read the slot without evaluating a relation itself.

## Recursive relations

**Recursive binding**  
A binding defined by a base relation and a relation that refers back to the
binding's current frontier.

**Anchor**  
The non-recursive base relation that seeds a recursive binding.

**Step (inductive term)**  
The relation evaluated from one frontier to produce candidate rows for the
next iteration.

**Frontier**  
The working relation produced by the preceding recursive iteration. A
recursive reference reads the frontier, not the entire accumulated result.

**Fixpoint**  
The completed recursive result reached when evaluating another step produces
no rows for a new frontier.

**Accumulation**  
The policy for admitting step rows to the result and the next frontier. `all`
admits every generated row; `new` admits only rows not already present by
canonical full-row identity.

## Storage and transactions

**Primary key**  
The column or columns that uniquely identify a stored table row. Their encoded
values also locate the row in primary storage.

**Index**  
An auxiliary ordering of selected column values that points back to table
rows. It provides an alternative access path for equality and range searches.

**Foreign key**  
A constraint requiring selected values in one row to identify an existing row
in another table. Delete restrictions prevent a referenced row from being
removed while dependent rows remain.

**Snapshot**  
The consistent storage view against which reads are evaluated. Reads from one
snapshot do not observe a mixture of unrelated storage states.

**Transaction**  
A group of reads and writes that commits atomically: either all of its effects
become visible or none of them do.
