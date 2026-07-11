// Package rad is a proof-of-concept relational database layer over an
// ordered key-value store (SlateDB), CockroachDB-style.
//
// Layer architecture — dependencies only flow downward:
//
//	01_kv       storage: KV + transaction primitives, key encoding,
//	            SlateDB adapter
//	            <- may not import anything above
//	02_catalog  schema: tables, columns, indexes, constraints
//	            <- imports 01
//	03_qir      the relation-graph query IR: values, types, unbound and
//	            bound relations/expressions, three-valued logic, result
//	            datums; storage-free, index-free, strategy-free
//	            <- imports 02 only for the shared type vocabulary
//	04_planner  bound QIR -> physical plan: binding, analyses, access
//	            paths, correlation classification
//	            <- imports 02 + 03
//	05_exec     physical plan -> KV operations: scans, index lookups,
//	            joins, constraint-checked writes, transactions
//	            <- imports 01 + 02 + 03 + 04
//	06_frontend public query interface; future SQL/GraphQL/ORM codecs
//	            lower into the IR here
//	            <- imports 01 + 02 + 03 + 05
//
// Types are shared upward (a layer may use types defined below it), never
// outward or downward. The directory names carry ordering numbers because Go
// package names cannot start with a digit; the declared package names are
// kv, catalog, qir, planner, exec, and frontend.
package rad
