# Rad QIR — Query IR and its Mapping to Key-Value Operations

> Status: **descriptive specification of the implementation as it stands.** This
> document is written to be lifted, later, into a standalone QIR specification
> and backed by an aggressive conformance suite. Where the current engine takes
> a POC shortcut, it is called out explicitly as a **[POC]** deviation so the
> future spec can decide whether to keep or close it.

The QIR ("Query IR") is the shape a Rad client sends over the wire to describe
a read, plus the sibling command IR for writes. This manual specifies:

1. the QIR grammar and its type model;
2. how it lowers through the engine layers;
3. the physical key-value layout it targets — to the byte;
4. the exact KV operations each QIR construct issues;
5. transaction, isolation, and result-shape semantics;
6. the invariants a conformance suite must pin down.

Everything here is traceable to code. Layer references use the engine's
downward-import stack:

```
06_frontend   public API (DB.Read / ReadJSON / Create / Update / Delete / Migrate)
05_exec       physical execution: access paths, KV ops, row/index codecs, mutations
04_planner    QIR -> physical plan; access-path selection
03_lir        the IR itself (shaped read + relational algebra) and typed values
02_catalog    schema model + persistence (tables, columns, indexes, FKs, defaults)
01_kv         ordered KV abstraction + order-preserving key encoding (over SlateDB)
```

Imports only ever point downward; nothing in `rad/` knows about the server or
CLI (those live in `cmd/rad`, which owns the wire<->IR lowering in
`wireconv.go`).

---

## 1. The pipeline

A read travels top to bottom; results are reassembled bottom to top.

```
  generated client (Go / TS)
        │  builds a protocol.Read value
        ▼
  POST /query  ─────────────  the rad:// wire protocol (protocol.Read, JSON)
        │
        ▼
  wireconv.toRead            wire -> lir.Read; JSON values coerced to column types
        │                    (cmd/rad/wireconv.go)
        ▼
  planner.PlanRead           lir.Read -> ShapedRead
        │                    - chooseAccess: pick PK lookup / index scan / full scan
        │                    - validate filter/order/aggs; resolve includes
        ▼
  exec.runShapedRead         ShapedRead -> []*Record
        │                    - fetchRows via the chosen access path (KV Get/Scan)
        │                    - filter, sort, offset, limit  (in memory)
        │                    - attachIncludes  (recursive, per-row relationship fetch)
        │                    - foldAggs         (if this is an aggregate)
        ▼
  01_kv over SlateDB         Get / Put / Delete / Scan([start,end))  on encoded keys
        │
        ▼
  frontend.RecordsJSON       []*Record -> nested JSON (objects, arrays, scalars)
        │
        ▼
  QueryResponse.records      back over the wire
```

The load-bearing idea, repeated throughout: **the access path is only an
optimization that narrows which keys are scanned; the filter remains the source
of truth and is re-evaluated on every fetched row.** A spec/test must never
assume a row was excluded because the access path skipped it.

---

## 2. Data and type model

### 2.1 The four types

Rad has exactly four scalar types (`02_catalog/types.go`):

| Internal type | `schema.rad` keyword | Go carrier | JSON on the wire |
|---------------|----------------------|-----------|------------------|
| `text`    | `string` | `string`  | string           |
| `int64`   | `int64`  | `int64`   | JSON number (decoded with full 64-bit precision) |
| `float64` | `float64`| `float64` | JSON number      |
| `bool`    | `bool`   | `bool`    | boolean          |

Note the one keyword mismatch: authors write `type: string` in `schema.rad`,
but the internal `catalog.Type` — and therefore what `/tables` introspection
reports and what this spec uses below — is `text` (`schema.go` maps `string` →
`TypeText` at parse time). The other three keywords are identical inside and
out.

Any column may be **nullable**; a value is then either one of the four types or
NULL. NULL is a first-class value in both the key encoding and the runtime value
model, and it sorts **before** every non-null value everywhere.

`format` (`uuid`, `unix_ms`, `email`, …) is **semantic metadata only**. The
engine never interprets it; it exists for codegen and tooling. Two columns that
differ only in `format` are physically identical.

### 2.2 The runtime value — `lir.Value`

`03_lir/value.go` defines the datum that flows through planning and execution:

```go
type Value struct {
    Type    catalog.Type // which field below is meaningful
    Text    string
    Int64   int64
    Float64 float64
    Bool    bool
    Null    bool         // overrides the payload
}
type Row = map[string]Value   // column name -> value
```

Two comparison operators define all ordering and equality semantics:

- `Value.Equal`: NULL never equals anything (including NULL); a type mismatch is
  never equal.
- `Value.Compare` → `-1 | 0 | 1`: **NULL sorts before every value**; `false <
  true`; comparing two different non-null types is an error (it cannot arise
  from a well-typed filter because literals are coerced to the column's type at
  lowering time).

`Value` imports only `catalog.Type`; the IR is otherwise self-contained.

---

## 3. The QIR wire grammar

Normative source: `protocol/protocol.go` (`Read`, `Expr`, `Order`, `Agg`,
`Include`) and the OpenAPI contract `protocol/openapi.yaml`. All reads — point
lookups, list queries, joins, and aggregations — are a single `Read` value sent
to `POST /query`. **There is no separate get/aggregate endpoint**; a point read
by primary key is just a `Read` filtered to the key columns with `limit 1`
(this is exactly what the generated `Get`/`By<Unique>` helpers build).

### 3.1 `Read`

```go
type Read struct {
    Table   string    // required
    Filter  *Expr     // optional predicate
    OrderBy []Order   // ordering terms, applied in list order
    Offset  int       // rows to skip
    Limit   int       // max rows; 0 = unlimited
    Include []Include // related relations to embed
    Aggs    []Agg     // present => fold matching rows to one scalar record
}
```

```json
{
  "table": "tasks",
  "filter": { "op": "eq", "column": "board_id", "value": "b1" },
  "order_by": [{ "column": "priority", "desc": true }],
  "limit": 20,
  "include": [
    { "fk": "tasks_assignee_id_fk", "dir": "parent", "as": "assignee" }
  ]
}
```

### 3.2 `Expr` — the filter AST

`Expr` is a tagged union selected by `op`:

```
op ∈ { and, or, not, eq, ne, lt, lte, gt, gte, is_null }

and / or : { "op":"and", "exprs":[ <Expr>, ... ] }
not      : { "op":"not", "expr": <Expr> }
eq..gte  : { "op":"eq",  "column":"status", "value":"todo" }
is_null  : { "op":"is_null", "column":"assignee_id" }
```

Semantics (see §7.2 for the three-valued logic):

- Comparisons (`eq ne lt lte gt gte`) test `column <op> value`. Any comparison
  where either side is NULL is **false** — SQL three-valued logic collapsed to
  two-valued at the boundary. **[POC]**
- `is_null` is the *only* way to match NULLs.
- `and`/`or` take a list; `not` negates a single sub-expression.
- `value` is a JSON scalar coerced to the column's declared type at lowering
  (a wrong-type value is rejected with an `invalid` problem, not guessed).

### 3.3 `Order`

```go
type Order struct { Column string; Desc bool }
```

Ascending by default. **NULLs sort first ascending, last descending** (a direct
consequence of `Value.Compare` placing NULL below everything, with `Desc`
negating the comparison).

### 3.4 `Include` — relationship embedding

```go
type Include struct {
    FK      string    // foreign-key name that defines the relation
    Dir     string    // "parent" | "children"
    As      string    // output field name (unique among siblings)
    Filter  *Expr     // children only
    OrderBy []Order   // children only
    Limit   int       // children only
    Include []Include // nested, children only for further nesting
    Aggs    []Agg     // children only; fold children to one scalar object
}
```

- `dir: "parent"` follows an FK **on this relation** to the single row it
  references → a nested **object** (or `null` when the FK is NULL). Parent
  includes take **no** refinements (`filter`/`order_by`/`limit`/`aggs` are
  rejected — a parent is at most one row).
- `dir: "children"` follows an FK **on another table** back to the rows that
  reference this one → a nested **array**. May be refined with
  `filter`/`order_by`/`limit`/nested `include`.
- `aggs` on a children include folds the matched children to a single scalar
  object instead of an array; it is then mutually exclusive with
  `order_by`/`limit`/nested `include`.

### 3.5 `Agg` — folds

```go
type Agg struct { Fn string; Column string; As string }
fn ∈ { count, sum, avg, min, max }
```

`Aggs` is a **shape annotation on a relation, not a node in the `Expr` AST** —
the same relation-materialising slot that yields records (no aggs) yields a
scalar fold (aggs present). This symmetry is deliberate; see
`docs/design/aggregations.md`. At the root, `Aggs` is mutually exclusive with
`order_by`/`offset`/`limit`/`include` (a fold collapses the whole matching set
to one row).

Aggregate semantics:

| Fn      | Result type            | Empty input | Notes |
|---------|------------------------|-------------|-------|
| `count` | `int64` (never NULL)   | `0`         | `count()` counts rows; `count(col)` counts non-NULL values |
| `sum`   | column's numeric type  | `NULL`      | numeric columns only; NULLs skipped |
| `avg`   | `float64` (always)     | `NULL`      | numeric columns only; `sum/count` over non-NULL |
| `min`   | column's type          | `NULL`      | any type; text lexicographic; `false < true`; NULLs skipped |
| `max`   | column's type          | `NULL`      | as `min` |

Overflow is not detected. **[POC]**

---

## 4. Lowering: wire → IR

### 4.1 Two IR forms

`03_lir` contains **two** IR shapes:

1. **The shaped read** — `lir.Read` (`03_lir/read.go`). This is what the wire
   maps onto today. It is intentionally *declarative*: it names a table, a
   filter, ordering, pagination, includes, and aggs, and **names no access
   path**. The planner decides physical access.
2. **The relational algebra** — `lir.Query{Root RelNode}` (`03_lir/lir.go`):
   `Scan`, `IndexScan`, `Filter`, `Project`, `Join`, `Limit` over a sealed
   `Expr` AST. This is the composable, "sacred" form reserved for future
   frontends (a SQL/GraphQL compiler would emit this). The wire path does **not**
   currently produce it. It is documented in Appendix C for completeness.

Do not conflate them. The rest of this spec follows the shaped-read path, which
is the one the generated clients drive.

### 4.2 `protocol.Read` → `lir.Read` (`cmd/rad/wireconv.go`)

`toRead` performs a structural, catalog-driven lowering. The essential rule:
**every value is coerced to its column's declared type against the catalog, never
guessed from JSON.** A JSON number lowered against an `int64` column becomes an
`int64`; against a `float64` column, a `float64`.

| wire field | lir.Read field | transform |
|------------|----------------|-----------|
| `Table`    | `Table`        | verbatim |
| `Filter`   | `Filter`       | `toExpr` (recursive) |
| `OrderBy`  | `OrderBy`      | `{Column, Desc}` 1:1 |
| `Offset`, `Limit` | same    | verbatim |
| `Include`  | `Include`      | `toIncludes` (resolves child table, recurses) |
| `Aggs`     | `Aggs`         | `{Fn, Column, As}` structural; planner validates |

Expr lowering maps `op` strings to typed nodes, and this is where a subtle but
**spec-critical asymmetry** is introduced:

- `eq` → `lir.Eq{ Left: ColRef{column}, Right: Literal{value} }`
- `ne lt lte gt gte` → `lir.Cmp{ Op, ColRef{column}, Literal{value} }`
- `is_null` → `lir.IsNull{ ColRef{column} }`
- `and`/`or`/`not` → `lir.And`/`lir.Or`/`lir.Not`

Equality is its own node (`lir.Eq`); every inequality is a `lir.Cmp`. The column
reference is always **bare** (empty alias) inside a `Read`. As §6 shows, only
`lir.Eq` drives access-path selection — so the `eq`-vs-everything-else split here
is what decides which filters can use an index.

---

## 5. Physical storage model

Rad stores everything — catalog and data — in **one ordered KV keyspace** over
SlateDB. One Rad instance is exactly one database; there is no schema/database
hierarchy.

### 5.1 The KV abstraction (`01_kv/kv.go`)

Four operations, and no more:

```go
Get(ctx, key)              -> (value, found, error)
Put(ctx, key, value)       -> error            // overwrites; no separate Set
Delete(ctx, key)           -> error            // deleting a missing key is not an error
Scan(ctx, start, end)      -> (Iterator, error) // half-open [start, end), ascending
```

- **Scans are half-open `[start, end)`**, iterated in ascending lexicographic
  key order. `nil` start = from the beginning; `nil` end = to the end. There is
  no prefix primitive — a **prefix scan is `Scan(prefix, PrefixEnd(prefix))`**.
- **There is no batch API and no standalone snapshot handle.** The transaction
  *is* the snapshot and atomicity unit (§12).
- Iterator contract: `for it.Next() { it.Key(); it.Value() }`, then `it.Err()`,
  then `it.Close()`. **`Key()`/`Value()` are valid only until the next
  `Next()`** — callers that retain them must clone.

The package's load-bearing invariant: implementations **must preserve
lexicographic byte ordering of keys** so range scans return rows in tuple order.
The SlateDB adapter (`01_kv/kvslate`) satisfies this directly (SlateDB stores raw
bytes, iterates lexicographically) and pins `StartInclusive:true,
EndInclusive:false`.

### 5.2 Order-preserving key encoding (`01_kv/keyenc`)

Every value encodes to `[tag byte][payload]`. Tags order mixed types; within a
type the bytes sort lexicographically in value order; every encoding is
**self-delimiting** so tuples concatenate with **no separator**.

| Type    | Tag  | Payload | Total |
|---------|------|---------|-------|
| NULL    | `0x01` | — | 1 byte |
| bool    | `0x02` | `0x00` (false) / `0x01` (true) | 2 bytes |
| int64   | `0x03` | big-endian of `uint64(i) XOR 0x8000_0000_0000_0000` | 9 bytes |
| float64 | `0x04` | big-endian of IEEE-754 bits, transformed (below) | 9 bytes |
| text    | `0x05` | body (each `0x00` → `0x00 0xFF`) then terminator `0x00 0x01` | variable |

**int64** — flipping the sign bit maps the signed range onto unsigned so that
big-endian bytes sort in numeric order:

```
MinInt64  -> 03 00 00 00 00 00 00 00 00
-1        -> 03 7F FF FF FF FF FF FF FF
0         -> 03 80 00 00 00 00 00 00 00
1         -> 03 80 00 00 00 00 00 00 01
MaxInt64  -> 03 FF FF FF FF FF FF FF FF
```

**float64** — non-negative: set the sign bit; negative: invert all 64 bits.
This makes the ordering monotonic across the sign boundary. **NaN is rejected by
callers** (`05_exec/tuple.go`) — it can never appear in a key.

```
-1.0 -> 04 40 0F FF FF FF FF FF FF
 0.0 -> 04 80 00 00 00 00 00 00 00
 1.0 -> 04 BF F0 00 00 00 00 00 00
```

**text** — the escape/terminator scheme (CockroachDB-style) is what makes string
keys prefix-safe: an embedded `0x00` becomes `0x00 0xFF`, and the string ends
with `0x00 0x01`. Because `0x01 < 0xFF`, a string always sorts before any
extension of itself (`"app" < "apple"`), and no encoded string is a byte-prefix
of another.

```
""     -> 05 00 01
"eu"   -> 05 65 75 00 01
"a\0"  -> 05 61 00 FF 00 01
```

**`PrefixEnd(prefix)`** returns the exclusive upper bound for a prefix scan:
increment the last non-`0xFF` byte and truncate after it; return `nil`
("unbounded") if the prefix is empty or all `0xFF`. Every key with the prefix
lies in `[prefix, PrefixEnd(prefix))`.

### 5.3 The keyspace

Top-level namespaces are literal ASCII path strings; only the tuple segments are
encoded bytes.

```
/rad/catalog/meta/next_id                 -> decimal-ASCII monotonic ID counter
/rad/catalog/table/{table_id}             -> JSON of the Table struct
/rad/catalog/table_name/{table_name}      -> "{table_id}"   (name -> id lookup)

/rad/data/{table_id}/primary/{pk_tuple}   -> row value (JSON, see 5.5)

/rad/index/{table_id}/{index_id}/{indexed_tuple}{pk_tuple}
                                          -> value = {pk_tuple}
```

Key facts:

- **Physical identity is the table ID (`t1`), never the name.** Names live only
  in catalog metadata, so renaming a table rewrites one metadata key and zero
  rows.
- The `{pk_tuple}` is the primary-key columns encoded in PK order and
  concatenated with no separator. **Composite PKs** are just more segments;
  boundaries are unambiguous because each encoding is self-delimiting.
- An **index entry** is `indexed_tuple ++ pk_tuple` in the key, and the **value
  is the PK tuple** — the index is a covering pointer back to the base row.

### 5.4 Index entries; unique vs non-unique

Unique and non-unique indexes are **physically identical**: both use the
`IndexKey` layout above, always with the full PK appended. Because the PK suffix
is always present, duplicate indexed values still produce distinct keys — a
non-unique index never collides.

Uniqueness is enforced **at write time**, not by key shape: before writing, the
engine prefix-scans `/rad/index/{table}/{index}/{indexed_tuple}` and rejects the
write if any existing entry points at a *different* PK (§10.1). Under a
serializable transaction the scanned prefix range is tracked even when empty, so
two concurrent inserts of the same unique value conflict at commit.

**[POC]** NULLs participate in uniqueness like ordinary values (SQL would treat
distinct NULLs as non-conflicting).

### 5.5 Row values are column-ID-keyed JSON (`05_exec/rowcodec.go`)

The value stored at a data key is **JSON: a map from column ID → `lir.Value`** —
not a tuple, not keyed by name:

```json
{ "c2": {"type":"int64","int64":1},
  "c3": {"type":"text","text":"Al"},
  "c4": {"type":"text","null":true} }
```

- `MarshalRow` translates column name → ID on write; `UnmarshalRow` translates
  ID → name against the **current** table definition on read.
- A zero value serializes as just its type (`omitempty` on payload fields):
  `Int64(0)` → `{"type":"int64"}`. This round-trips because `Type`
  disambiguates.
- Every column is present in the stored map (`normalizeRow` fills absent
  nullable columns with explicit NULL before write).

This column-ID indirection is the second half of rename-safety: **column values
are keyed by stable ID, so a column rename rewrites catalog metadata and zero
rows.**

### 5.6 Stable IDs and rename/drop semantics (`02_catalog`, `05_exec/rowcodec.go`)

A single monotonic counter at `/rad/catalog/meta/next_id` issues every ID;
kind prefixes disambiguate and IDs are **never reused**:

```
tables -> t{n}     columns -> c{n}     indexes -> i{n}     foreign keys -> fk{n}
```

Consequences a conformance suite should assert:

- **Rename column**: updates the column `Name` and every name reference
  (primary key, index columns, FK columns) in catalog metadata. No row data is
  touched.
- **Rename table**: updates the `Name` field and the `table_name/*` lookup key.
  Data keys (which use the table ID) are untouched.
- **Add column**: existing rows have no field for the new column ID; on read
  they yield the column's **literal** default if defined, else NULL. A generator
  default (`uuid()`, `now_ms()`) is **never** fabricated on read — only at
  insert.
- **Drop column**: removed from the catalog; the orphaned field in existing row
  JSON is ignored on read. Re-adding gets a fresh ID, so old data does not
  resurrect.

---

## 6. Access-path selection (`04_planner`)

`PlanRead` turns `lir.Read` into a `ShapedRead`:

```go
type ShapedRead struct {
    Table   catalog.Table
    Access  Access            // the chosen physical path
    Filter  lir.Expr          // the FULL residual predicate (source of truth)
    OrderBy []lir.OrderTerm
    Offset, Limit int
    Include []ShapedInclude
    Aggs    []lir.AggTerm
}
type Access struct {
    Kind   AccessKind         // full_scan | pk_lookup | index_scan
    Key    lir.Row            // pk_lookup: the complete PK
    Index  catalog.Index      // index_scan: the chosen index
    Prefix lir.Row            // index_scan: leading equality columns
}
```

Notably there is **no Sort/Filter/Limit/Aggregate plan node** in this path —
ordering, pagination, the residual filter, and aggregation are all *fields* the
executor interprets. Only the access path is a resolved physical choice.

### 6.1 `chooseAccess` — the whole of it

```go
eqs := equalities(filter)                     // column = non-null literal, under top-level ANDs
if len(eqs) == 0            -> AccessFullScan
if covers(eqs, PrimaryKey)  -> AccessPKLookup(Key = all PK columns)
else                        -> AccessIndexScan on the index with the longest
                               gapless leading equality prefix (n > 0);
                               if none, AccessFullScan
```

`equalities` is the **only** signal, and it is deliberately narrow:

- It recurses **only** through `lir.And`. An `Or`, `Not`, `Cmp`, or `IsNull`
  node stops the walk and contributes nothing.
- It matches **only** `lir.Eq` whose left is a **bare `ColRef`** and whose right
  is a **non-NULL `Literal`**. Orientation is fixed: `literal = column` is not
  recognised; a NULL literal is ignored.
- Because `ne`/`lt`/`lte`/`gt`/`gte` lowered to `lir.Cmp` (not `lir.Eq`), **no
  inequality or range predicate ever selects an index.** Ranges survive only as
  residual filter. **[POC]** — range index scans are a known future extension.

`covers(eqs, pk)` is true iff the PK is non-empty and every PK column appears in
`eqs` (extra equalities are allowed). Index prefix matching counts leading index
columns present in `eqs`, stopping at the first gap; the longest wins, ties
resolve to the first index declared.

### 6.2 Decision table

| Filter shape (bare columns) | Access |
|-----------------------------|--------|
| absent / only inequalities / top-level `or` / top-level `not` / `is_null` | full scan |
| all PK columns `= literal` (possibly with extras) | PK lookup |
| leading columns of some index `= literal` (gapless, `n ≥ 1`) | index scan on that index (prefix len `n`) |
| a non-leading indexed column `=` only | full scan (cannot use the index) |

**The residual filter is the *entire* original predicate** — nothing is stripped
after path selection. Even the equality that chose the index is re-checked on
every fetched row. A test must treat the access path as advisory and the filter
as authoritative.

---

## 7. Read execution (`05_exec/read.go`)

`Engine.Read` (committed state) / `Tx.Read` (transaction snapshot) → `runShapedRead`:

```
1. fetchRows(view, table, Access)          -- locate candidate rows via the access path
2. if Aggs:  applyShaping(rows, Filter, nil, 0, 0); return foldAggs(rows)   -- one Record
   else:     applyShaping(rows, Filter, OrderBy, Offset, Limit)
             for each surviving row: attachIncludes -> Record
```

### 7.1 `fetchRows` — access path to KV ops

| Access | KV operations | Key range |
|--------|---------------|-----------|
| **pk_lookup** | one `Get` | `DataKey(table_id, pk_tuple)` — 0 or 1 row |
| **full_scan** | one `Scan`, iterate all | `[DataPrefix, PrefixEnd(DataPrefix))` = every row in PK order |
| **index_scan** | one `Scan` + one `Get` **per matching entry** | `[IndexPrefix++prefix_tuple, PrefixEnd(...))`, collect PK tuples, then `Get(DataKey(pk))` each |

So **an index scan over N matches costs 1 Scan + N Gets** (the index stores PK
pointers, then the base rows are fetched individually). A missing base row for a
live index entry is a hard error ("index points at missing row") — an integrity
invariant the suite should exercise.

**Everything materialises**: `fetchRows` returns a full `[]lir.Row`; there is no
predicate or limit pushdown into KV. The access path only narrows the scanned
key range. **[POC]**

### 7.2 Filter evaluation and three-valued logic

Filtering runs **in memory, after fetch**, over decoded (unqualified) rows.
`evalRead` walks the `Expr` tree: `And`/`Or`/`Not`, `IsNull`, `Eq`, `Cmp`.
Values come from the row (`ColRef`) or the literal.

The NULL rule, collapsed to two-valued logic **[POC]**:

- Any comparison (`Eq`/`Cmp`) with a NULL operand is **false**.
- `Not(false-because-null)` is therefore **true**.
- `IsNull` is the only predicate that returns true for a NULL.

### 7.3 Ordering, offset, limit

All applied in memory, after filtering, in this order:

1. **Order** — stable sort; for each `OrderTerm`, compare `a[col].Compare(b[col])`,
   negated for `Desc`. NULLs sort first ascending / last descending.
2. **Offset** — reslice `rows[offset:]` (empty if offset ≥ len).
3. **Limit** — `rows[:limit]` when `limit > 0 && len > limit`.

The full matching set is sorted before offset/limit apply (no top-N pushdown).
**[POC]**

---

## 8. Relationships (`05_exec/read.go` `attachIncludes`)

There is **no join operator** in this path. Nesting is a **recursive,
per-parent-row relationship fetch** — a classic N+1 traversal. For each surviving
root row, `attachIncludes` builds a `Record{Columns: row}` and, per planned
include, dispatches on direction.

### 8.1 Parent include (`dir: "parent"`)

The FK lives on the current row. Build the referenced PK by mapping the child
row's FK columns to the parent's referenced columns.

- If **any FK column is NULL** → the parent is `null`; **no KV op**.
- Otherwise → one `Get` on the parent's `DataKey`, then recurse into the
  parent's own includes. Stored on `rec.Parents[as]`.

### 8.2 Children include (`dir: "children"`)

The FK lives on another table referencing this row. Build a `want` row mapping
the child's FK columns to this row's referenced values, then:

- If the planner attached a **`ChildIndex`** (an index on the child whose
  **leading columns exactly equal the FK columns**, in order): index scan →
  **1 Scan + one Get per matching child**.
- Otherwise **fallback**: **full scan of the entire child table**, filtered in
  memory to rows where every `want` column equals. **[POC]** — a children fetch
  with no covering index scans the whole child table per parent row.

Then `applyShaping(childRows, inc.Filter, inc.OrderBy, 0, inc.Limit)` — note
**offset is always 0 for children** — and recurse. Stored on `rec.Children[as]`.

### 8.3 The N+1 model

Every parent row triggers its own independent scan/Get for each relationship;
**there is no batching across sibling rows.** This is a deliberate POC
simplification (correctness over cost). A conformance suite should assert the
*results*, and a separate performance suite can assert the *op counts* below.

### 8.4 Worked KV-op trace

Query: board `b1`, include its `tasks` (children), each task's `assignee`
(parent) and `comments` (children), against the Tracker schema (tasks indexed by
`[board_id, status]`; comments have `task_id index:true`; task `assignee_id`
nullable):

```
Get   /rad/data/boards/primary/{b1}                        -- root: PK lookup (1 board)
Scan  /rad/index/tasks/{i}/{board_id=b1}…                  -- tasks: board_id is the leading
Get   /rad/data/tasks/primary/{t1}                            index column -> index scan
Get   /rad/data/tasks/primary/{t2}                            (1 Scan + 3 Gets)
Get   /rad/data/tasks/primary/{t3}
  -- sort tasks by chosen order in memory, then per task:
  t1: Get /rad/data/users/primary/{ada}                    -- assignee (non-null FK): 1 Get
      Scan /rad/index/comments/{i}/{task_id=t1}…           -- comments: 1 Scan, 0 matches -> 0 Gets
  t2: Get /rad/data/users/primary/{bob}                    -- assignee: 1 Get
      Scan /rad/index/comments/{i}/{task_id=t2}…           -- comments: 1 Scan + 2 Gets
      Get /rad/data/comments/primary/{c1}; Get …/{c2}
  t3: (assignee_id NULL -> no KV op; parent = null)
      Scan /rad/index/comments/{i}/{task_id=t3}…           -- 0 matches -> 0 Gets
```

---

## 9. Aggregation (`05_exec/aggregate.go`)

`foldAggs` is a **single fold over the already-fetched-and-filtered rows**, one
pass per term. The planner has pre-validated every term (column exists;
`sum`/`avg` numeric; distinct non-empty `As`), so the fold trusts them.

Two entry points:

- **Root aggregate**: access path is chosen from the filter exactly as for a
  record read (a `count` filtered by PK still rides a PK lookup), rows are
  filtered, then folded into one `Record{Columns: scalars}`. `order_by` /
  `offset` / `limit` / `include` are rejected at plan time with a human-worded
  error.
- **Children aggregate include** (`foldChildren`): same child fetch as a normal
  children include (index scan or full-scan-filter), filtered, then folded into
  `rec.Scalars[as]` — a single scalar **object**, not an array. Nested
  include/order/limit on an aggregate include are rejected at plan time; parent
  aggregates are rejected outright.

Typing and empty-set rules are in §3.5. **[POC]** the fold still materialises the
row slice it folds — a genuinely streaming fold is a later optimisation, not a
correctness concern.

---

## 10. Mutations (command IR)

Writes are the sibling command IR (`/create`, `/update`, `/delete`). Every write
runs inside a **`SerializableSnapshot` transaction** (§12), so the row write, its
index entries, and its constraint checks commit atomically and race-safely.

### 10.1 Create (`05_exec/engine.go` `insert`)

```
applyDefaults      -- fill absent columns that have a default (§10.4)
normalizeRow       -- reject unknown columns; fill absent nullable -> explicit NULL;
                      absent non-nullable -> error; type-check
Get  DataKey(pk)                     -- duplicate-PK check (present -> error)
Get  DataKey(refTable, parentPK)     -- per FK with all-non-null columns: parent must exist
Scan IndexPrefix++indexedTuple       -- per unique index: reject if an entry points at another PK
Put  DataKey(pk) = MarshalRow(row)   -- write the row
Put  IndexKey(idx, indexedTuple, pk) = pk   -- for EVERY index (unique or not)
```

Net: `1 Get + (Get per non-null FK) + (Scan per unique index) + 1 Put + (Put per
index)`. FK columns that are NULL skip the parent check (SQL semantics). FK
targets must be the parent's full primary key (enforced at schema-creation time).

### 10.2 Update (`05_exec/mutate.go` `update`)

```
Get DataKey(pk)                      -- load current; absent -> found=false, no write
                                        reject unknown columns; reject any PK column in `set`
                                        (the PK is immutable)
merge current+set -> normalizeRow    -- clearing to NULL: pass Null(type) in `set`;
                                        defaults are NOT re-applied on update
re-check only AFFECTED constraints   -- FKs / unique indexes whose columns the patch touches
Put DataKey(pk) = row                -- PK unchanged, so the row key is stable
per index whose columns changed:     -- index maintenance
   Delete IndexKey(oldTuple); Put IndexKey(newTuple)
```

Indexes untouched by the patch are left alone; an index whose recomputed tuple is
unchanged is skipped.

### 10.3 Delete (`05_exec/mutate.go` `delete`)

```
Get DataKey(pk)                      -- absent -> found=false
FK-restrict check                    -- scan ALL tables for any FK referencing this table;
                                        if any referencing row exists -> error (no cascade)
   (uses a child index prefix scan if the FK columns lead an index, else full child scan;
    a self-referential delete skips the row being deleted)
per index: Delete IndexKey(...)      -- remove every index entry
Delete DataKey(pk)                   -- remove the row
```

**[POC]** deletes **restrict**, never cascade.

### 10.4 Defaults (`05_exec/defaults.go`)

Applied **at insert only**. For each catalog column that is **absent** from the
input and has a `Default`:

- `uuid()` → a random RFC-4122 v4 string (`crypto/rand`).
- `now_ms()` → `time.Now().UnixMilli()` (int64).
- a literal → the typed literal from the column's default.

**Explicit values win, including an explicit NULL** — a present key is never
overwritten by a default. Generator defaults are never fabricated on read (§5.6).

---

## 11. Migration and backfill (`06_frontend/migrate.go`, `05_exec/backfill.go`)

`DB.Migrate(desired)` diffs the current catalog against the desired schema and
applies each step in order (add/drop table, add/drop column, rename via
`renamed_from` hints, add/drop index). Migration is idempotent; a schema that
already matches applies nothing.

- **Add index**: registers the index metadata, then `BackfillIndex` scans every
  existing row and `Put`s an index entry for each — all in one transaction. For a
  **unique** index it tracks seen tuples and **fails the whole migration** if the
  existing data already violates uniqueness. **[POC]** a failed backfill leaves
  the index registered but unpopulated; rerunning retries the backfill.
- **Add column**: no row rewrite — existing rows read the new column back as its
  literal default or NULL (§5.6).

---

## 12. Transactions and isolation (`01_kv`, `05_exec/engine.go`)

The transaction is Rad's unit of snapshot and atomicity. Two isolation levels:

- **Snapshot**: reads see a stable snapshot taken at `Begin` overlaid with the
  txn's own writes; detects **write-write** conflicts only (write skew is
  possible).
- **SerializableSnapshot**: additionally detects **read-write** conflicts —
  point reads *and the requested bounds of scans* are validated, so a phantom
  insert into a scanned range conflicts even if the scan returned nothing.

All mutations run at `SerializableSnapshot`. This is what makes the
constraint checks in §10 race-safe: the FK-parent `Get` and the unique-index
prefix `Scan` are tracked reads, so a concurrent violation surfaces as a
commit-time conflict rather than a corrupt write.

- Reads see their own buffered writes; scans reflect the txn's own Puts/Deletes.
- Commit awaits durability; it either applies atomically or returns an error
  wrapping **`kv.ErrConflict`** (retry with fresh reads, or abandon). The txn is
  spent after commit either way. Iterators must be closed before commit.
- Rollback is idempotent and a no-op after commit — `defer tx.Rollback()` is
  always safe.

The conformance suite `01_kv/kvtest` is the executable definition of these
semantics (commit visibility, rollback, snapshot reads, scan-sees-own-writes,
write-write conflict, read-key conflict, phantom-range conflict, write-skew
allowed under Snapshot / blocked under SerializableSnapshot).

---

## 13. Result shape (`06_frontend/read.go`)

A read returns `[]*Record`; `RecordsJSON` renders each into one flat JSON object
by merging four maps of the `Record`, dispatching by relation kind:

```go
type Record struct {
    Columns  lir.Row                // scalar fields
    Parents  map[string]*Record     // parent include -> nested object (or null)
    Children map[string][]*Record   // children include -> array of objects
    Scalars  map[string]lir.Row     // aggregate include -> nested scalar object
}
```

- `Columns` → scalar fields (`text`→string, `int64`→int64, `float64`→float64,
  `bool`→bool, NULL→JSON `null`).
- `Parents[as]` → nested object, recursively shaped; a nil pointer (NULL FK) →
  JSON `null`.
- `Children[as]` → JSON array of recursively-shaped objects.
- `Scalars[as]` → nested object of the folded aggregate values.

A **root aggregate** returns a single `Record` whose `Columns` are the folded
scalars and whose relation maps are empty — it encodes as one flat object.

Rad **never** exposes a flattened SQL-style join; the result is always shaped
like the request. `MarshalRecords` sorts keys for deterministic output.

---

## 14. Worked end-to-end example (to the byte)

Schema (IDs from the shared counter):

```
users  id=t1   columns: id (c2, int64, PK), name (c3, text), email (c4, text)
       primary_key = [id]
       index users_email_idx (i5, columns=[email], unique)
```

Catalog KV:

```
/rad/catalog/meta/next_id          = "5"
/rad/catalog/table_name/users      = "t1"
/rad/catalog/table/t1              = {"id":"t1","name":"users","columns":[
                                       {"id":"c2","name":"id","type":"int64"},
                                       {"id":"c3","name":"name","type":"text"},
                                       {"id":"c4","name":"email","type":"text"}],
                                      "primary_key":["id"],
                                      "indexes":[{"id":"i5","name":"users_email_idx",
                                                  "columns":["email"],"unique":true}],
                                      "foreign_keys":[]}
```

Insert `{id:1, name:"Al", email:"a@b.co"}`. PK tuple for `id=1`:
`03 80 00 00 00 00 00 00 01`.

```
Row:
  key /rad/data/t1/primary/ ++ 03 80 00 00 00 00 00 00 01
  val {"c2":{"type":"int64","int64":1},
       "c3":{"type":"text","text":"Al"},
       "c4":{"type":"text","text":"a@b.co"}}

Index entry (email = "a@b.co" -> 05 61 40 62 2E 63 6F 00 01):
  key /rad/index/t1/i5/ ++ 05 61 40 62 2E 63 6F 00 01 ++ 03 80 00 00 00 00 00 00 01
  val 03 80 00 00 00 00 00 00 01                          (the PK tuple)
```

Query `{table:"users", filter:{op:"eq",column:"email",value:"a@b.co"}}`:

```
chooseAccess: eqs = {email: "a@b.co"}; PK not covered; users_email_idx leads with
              email (n=1) -> AccessIndexScan(index=i5, prefix={email})
fetchRows:    Scan /rad/index/t1/i5/ ++ 05 61 40 62 2E 63 6F 00 01 … [half-open]
              -> collects PK tuple 03 80 …01
              Get /rad/data/t1/primary/ ++ 03 80 …01 -> the row
applyShaping: re-evaluate the full filter (email == "a@b.co") on the fetched row -> kept
result:       [{ "id": 1, "name": "Al", "email": "a@b.co" }]
```

Query `{table:"users", filter:{op:"eq",column:"id",value:1}}` instead:

```
chooseAccess: eqs = {id: 1}; covers PK -> AccessPKLookup(Key={id:1})
fetchRows:    Get /rad/data/t1/primary/ ++ 03 80 …01     (single Get, no scan)
```

---

## 15. Invariants for a conformance suite

The properties the future QIR test harness should pin down, grouped:

**Encoding & ordering**
- For each type, `decode(encode(v)) == v`; encoded order == value order
  (exhaustive at boundaries + randomized).
- Cross-type tag order `null < bool < int64 < float64 < text`.
- String prefix-safety: no encoded string is a byte-prefix of another; embedded
  `0x00` survives round-trip.
- `PrefixEnd`: every key with a prefix lies in `[prefix, PrefixEnd(prefix))`;
  all-`0xFF`/empty → unbounded.

**Access-path selection (behavioural, not just structural)**
- Path choice never changes the result set — for any filter, full scan and the
  chosen path return identical rows. (The residual filter is authoritative.)
- Complete-PK equality → PK lookup; longest gapless leading index prefix → index
  scan; everything else → full scan.
- `or`, `not`, inequalities, `is_null`, reversed `literal = column`, and NULL
  literals never select an index.

**Read semantics**
- Three-valued logic: comparisons with NULL are false; `is_null` is the only
  NULL match; `not` of false-because-NULL is true.
- Ordering: NULLs first ascending / last descending; stable; multi-term.
- Offset/limit applied after sort; children offset is always 0.

**Relationships**
- Parent with NULL FK → `null` and no KV op; children with no covering index →
  correct results via full-scan fallback.
- Nested includes reassemble to the requested shape (object / array / scalar
  object); a root aggregate is one flat object.

**Aggregation**
- Per-fn typing and empty-set rules (§3.5), especially `count → 0` vs everyone
  else `→ NULL` on empty, and NULL-skipping.
- Root aggs reject order/offset/limit/include; children aggs reject
  order/limit/nested-include; parent aggs rejected.

**Mutations & integrity**
- Create: duplicate PK rejected; missing FK parent rejected; unique violation
  rejected; every index gets an entry.
- Update: PK immutable; clear-to-NULL; only touched indexes/constraints
  re-evaluated; stale index entry removed.
- Delete: FK-restrict blocks deletion of a referenced row; all index entries
  removed; self-reference handled.
- Every live index entry points at an existing base row.

**Transactions** — the whole of `01_kv/kvtest` (§12).

---

## 16. POC deviations and non-goals

Collected for the future spec to rule on. All are marked **[POC]** above:

- Three-valued logic is collapsed to two-valued at the filter boundary
  (comparisons with NULL are false; no `UNKNOWN`).
- NULLs participate in unique constraints as ordinary values.
- Only equality predicates drive access paths; range/inequality index scans are
  not implemented (ranges are residual-filtered).
- No predicate/limit/offset pushdown — reads materialise the full matching set,
  then filter/sort/paginate in memory.
- Includes are unbatched N+1; a children include with no covering index
  full-scans the child table per parent row.
- Aggregates materialise before folding (no streaming); overflow undetected.
- Deletes restrict, never cascade.
- A failed unique-index backfill leaves the index registered but empty.

Out of scope entirely (per `docs/v0-spec.md`): `GROUP BY`/`HAVING`/`DISTINCT`,
window functions, recursive queries, CTEs, unions, cost-based optimisation,
statistics, join reordering, an expression language beyond the above, and any
SQL endpoint.

---

## Appendix A — encoding reference

```
NULL     01
bool     02 {00|01}
int64    03 <8 bytes BE of (uint64(i) XOR 0x8000000000000000)>
float64  04 <8 bytes BE of IEEE754 bits; non-neg: set sign bit; neg: invert all>   (NaN forbidden)
text     05 <body, each 0x00 -> 0x00 0xFF> 00 01
tuple    <enc(v1)><enc(v2)>…   (no separator; each value self-delimiting)
prefix scan  Scan(prefix, PrefixEnd(prefix))     [half-open, ascending]
```

## Appendix B — key/value reference

```
catalog counter   /rad/catalog/meta/next_id                       -> decimal ASCII
catalog table      /rad/catalog/table/{table_id}                   -> Table JSON
catalog name index /rad/catalog/table_name/{name}                  -> "{table_id}"
row                /rad/data/{table_id}/primary/{pk_tuple}         -> {col_id: Value} JSON
index entry        /rad/index/{table_id}/{index_id}/{idx_tuple}{pk_tuple} -> {pk_tuple}
```

## Appendix C — the relational-algebra IR (reserved, not on the wire path)

`03_lir/lir.go` defines a general algebra that the wire path does **not** emit
today; it is the composable form a future SQL/GraphQL frontend would lower into,
planned by `planner.Plan` → `planner.Node` (physical `TableScan`, `IndexScan`,
`Filter`, `Project`, `NestedLoopJoin`, `Limit`).

```
Query{ Root RelNode }
RelNode  = Scan{Table,Alias}
         | IndexScan{Table,Alias,Index,Prefix}   -- names a physical path; a wart to
         |                                           migrate out once the planner owns
         |                                           access selection for this path too
         | Filter{Input,Expr}
         | Project{Input,[]Projection}
         | Join{Left,Right,InnerJoin,On}          -- inner only; nested-loop when planned
         | Limit{Input,N}
Expr     = Eq | Cmp{Ne|Lt|Lte|Gt|Gte} | And | Or | Not | IsNull | ColRef | Literal
```

The design intent (see the QIR design principle: *evolve the IR as composable
relation shapes; don't freeze the abstraction*) is that `Read`/`Include` and this
algebra converge over time — an include is already "materialise a related shape,"
and `Aggs` is a cardinality of that same idea — rather than the taxonomy being
designed up front. This appendix documents the algebra's existence so the QIR
spec can decide the convergence deliberately.
