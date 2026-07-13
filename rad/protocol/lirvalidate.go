package protocol

// Best-match validation errors. The LIR schema's Node and Expr unions are
// closed oneOf sets, so a raw validator failure is a fan-out over every
// variant — correct, and useless to a caller. But `kind` discriminates the
// union: when validation fails, re-validate each node (and each kinded
// expression object inside it) against exactly the variant its kind names,
// and report that variant's specific error with the node id attached. No
// schema change: the variants are already named $defs.

import (
	"encoding/json"
	"fmt"
	"maps"
	"slices"
	"sync"

	"github.com/google/jsonschema-go/jsonschema"
)

// kindDefs maps a wire `kind` to its named variant in the schema's $defs.
var kindDefs = map[string]string{
	"scan":      "ScanNode",
	"filter":    "FilterNode",
	"project":   "ProjectNode",
	"join":      "JoinNode",
	"aggregate": "AggregateNode",
	"order":     "OrderNode",
	"slice":     "SliceNode",
	"ref":       "RefNode",

	"lit":    "LiteralExpr",
	"col":    "ColumnExpr",
	"unary":  "UnaryExpr",
	"binary": "BinaryExpr",
	"call":   "CallExpr",
	"cast":   "CastExpr",
	"exists": "CrossingExprExists",
	"first":  "CrossingExprFirst",
	"scalar": "CrossingExprScalar",
	"array":  "CrossingExprArray",
}

var nodeKinds = map[string]bool{
	"scan": true, "filter": true, "project": true, "join": true,
	"aggregate": true, "order": true, "slice": true, "ref": true,
}

// resolveVariants compiles one standalone resolved schema per kind: the
// embedded document's $defs with the variant as the root reference.
var resolveVariants = sync.OnceValues(func() (map[string]*jsonschema.Resolved, error) {
	out := make(map[string]*jsonschema.Resolved, len(kindDefs))
	for kind, def := range kindDefs {
		var doc jsonschema.Schema
		if err := json.Unmarshal(lirSchemaJSON, &doc); err != nil {
			return nil, fmt.Errorf("protocol: decode embedded LIR schema: %w", err)
		}
		wrapper := &jsonschema.Schema{Defs: doc.Defs, Ref: "#/$defs/" + def}
		resolved, err := wrapper.Resolve(nil)
		if err != nil {
			return nil, fmt.Errorf("protocol: resolve LIR variant %s: %w", def, err)
		}
		out[kind] = resolved
	}
	return out, nil
})

// validationDetail turns a raw oneOf fan-out failure into the most specific
// message available: the failing node's kind-matched variant error, drilled
// into the first failing kinded expression when there is one. When nothing
// node-shaped explains the failure (bad root, bad top-level shape), the
// original error stands.
func validationDetail(instance any, original error) error {
	variants, err := resolveVariants()
	if err != nil {
		return original
	}
	doc, ok := instance.(map[string]any)
	if !ok {
		return original
	}
	nodes, ok := doc["nodes"].(map[string]any)
	if !ok {
		return original
	}

	for _, id := range slices.Sorted(maps.Keys(nodes)) {
		node, ok := nodes[id].(map[string]any)
		if !ok {
			return fmt.Errorf("protocol: node %q: not an object", id)
		}
		kind, _ := node["kind"].(string)
		if kind == "" {
			return fmt.Errorf("protocol: node %q: missing \"kind\"", id)
		}
		if !nodeKinds[kind] {
			return fmt.Errorf("protocol: node %q: unknown relation kind %q", id, kind)
		}
		if err := variants[kind].Validate(node); err != nil {
			// The node's variant failed — see whether a kinded expression
			// inside it explains the failure more precisely.
			if inner := firstExprFailure(variants, node); inner != nil {
				return fmt.Errorf("protocol: node %q (%s): %w", id, kind, inner)
			}
			return fmt.Errorf("protocol: node %q (%s): %w", id, kind, err)
		}
	}
	return original
}

// firstExprFailure walks a node payload for objects carrying an expression
// kind and validates each against its variant, returning the first failure.
func firstExprFailure(variants map[string]*jsonschema.Resolved, v any) error {
	switch x := v.(type) {
	case map[string]any:
		if kind, _ := x["kind"].(string); kind != "" && !nodeKinds[kind] {
			variant, known := variants[kind]
			if !known {
				return fmt.Errorf("unknown expression kind %q", kind)
			}
			if err := variant.Validate(x); err != nil {
				return fmt.Errorf("expression (%s): %w", kind, err)
			}
			// The expression's own shape is fine; its children may not be.
		}
		for _, key := range slices.Sorted(maps.Keys(x)) {
			if err := firstExprFailure(variants, x[key]); err != nil {
				return err
			}
		}
	case []any:
		for _, el := range x {
			if err := firstExprFailure(variants, el); err != nil {
				return err
			}
		}
	}
	return nil
}
