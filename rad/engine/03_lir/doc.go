// Package lir is Rad's canonical query IR: the relation graph.
//
// The IR has exactly two categories. A Relation is a possibly-empty,
// possibly-many stream of structurally typed rows; operators consume
// relations and produce relations. An Expr computes exactly one typed datum
// in some scope; expressions may consume relations only through an
// explicit cardinality-crossing operator (Exists, First, Scalar, Array) that
// states how many rows become one datum. Result shaping is projection: a Project field
// whose expression is a crossing renders as a nested object, array, scalar,
// or boolean. Relationships are not a separate kind of query — they are
// ordinary correlated relations materialised into output shapes.
//
// This package defines the UNBOUND form: table, column, and scope names plus
// raw literals, exactly as a frontend produces it. The binder (04_planner)
// resolves it against the catalog into the bound form (lir/bound), where
// every relation output is addressed by dense slots. Planning and execution
// trust bound IR completely.
//
// The discipline, before adding any node here: could this be expressed as an
// ordinary relation? If yes, resist the special node. The normative design —
// grammar, three-valued logic, determinism rules, the wire format — is
// docs/design/lir.md.
package lir
