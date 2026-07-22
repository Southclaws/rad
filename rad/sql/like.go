package sql

import (
	"fmt"
	"strings"
	"unicode/utf8"

	"github.com/pgplex/pgparser/nodes"

	"github.com/Southclaws/rad/rad/protocol/lirwire"
)

// lowerLike compiles PostgreSQL [NOT] LIKE/ILIKE into the structured LIR
// text matcher. Patterns must be statement constants (literal or bound
// parameter), because TextMatchExpr deliberately has no per-row pattern
// operand. '%' becomes any_many; escaped wildcard characters remain literal.
// The '_' single-character wildcard remains unsupported until LIR defines
// what one text character means.
func (c *cc) lowerLike(e *env, a *nodes.A_Expr) (lirwire.Expr, exprType, error) {
	name, err := operatorName(a.Name)
	if err != nil {
		return lirwire.Expr{}, exprType{}, err
	}
	negated := name == "!~~" || name == "!~~*"

	textT := exprType{scalar: lirwire.ScalarTypeText}
	value, valueType, err := c.lowerExpr(e, a.Lexpr, &textT)
	if err != nil {
		return lirwire.Expr{}, exprType{}, err
	}
	if valueType.scalar != lirwire.ScalarTypeText {
		return lirwire.Expr{}, exprType{}, unsupportedf("LIKE on %s", valueType.scalar)
	}

	patternNode, escapeNode, err := likePatternNodes(a.Rexpr)
	if err != nil {
		return lirwire.Expr{}, exprType{}, err
	}
	patternExpr, patternType, err := c.lowerExpr(e, patternNode, &textT)
	if err != nil {
		return lirwire.Expr{}, exprType{}, err
	}
	pattern, patternNull, constant := textLiteralOf(patternExpr)
	if !constant {
		return lirwire.Expr{}, exprType{}, unsupportedf("non-constant LIKE pattern")
	}

	escape := '\\'
	escapeEnabled := true
	escapeNullable := false
	if escapeNode != nil {
		escapeExpr, escapeType, err := c.lowerExpr(e, escapeNode, &textT)
		if err != nil {
			return lirwire.Expr{}, exprType{}, err
		}
		escapeNullable = escapeType.nullable
		escapeText, escapeNull, constant := textLiteralOf(escapeExpr)
		if !constant {
			return lirwire.Expr{}, exprType{}, unsupportedf("non-constant LIKE escape")
		}
		if escapeNull {
			return lirwire.Lit(lirwire.Null(lirwire.ScalarTypeBool)), exprType{scalar: lirwire.ScalarTypeBool, nullable: true}, nil
		}
		switch utf8.RuneCountInString(escapeText) {
		case 0:
			escapeEnabled = false
		case 1:
			escape, _ = utf8.DecodeRuneInString(escapeText)
		default:
			return lirwire.Expr{}, exprType{}, fmt.Errorf("LIKE ESCAPE expression must be empty or one character")
		}
	}

	resultType := exprType{
		scalar:   lirwire.ScalarTypeBool,
		nullable: valueType.nullable || patternType.nullable || escapeNullable,
	}
	if patternNull {
		return lirwire.Lit(lirwire.Null(lirwire.ScalarTypeBool)), resultType, nil
	}

	parts, err := tokenizeLikePattern(pattern, escape, escapeEnabled)
	if err != nil {
		return lirwire.Expr{}, exprType{}, err
	}
	var out lirwire.Expr
	if len(parts) == 0 {
		// TextMatchExpr requires at least one non-empty part. The only pattern
		// producing none is the empty pattern, whose semantics are exact empty
		// equality under both LIKE and ILIKE.
		out = lirwire.Binary("eq", value, lirwire.Lit(lirwire.Text("")))
	} else {
		comparison := lirwire.TextComparisonExact
		if a.Kind == nodes.AEXPR_ILIKE {
			comparison = lirwire.TextComparisonUnicodeSimpleFold
		}
		out = lirwire.TextMatchWithComparison(value, comparison, parts...)
	}
	if negated {
		out = lirwire.Unary("not", out)
	}
	return out, resultType, nil
}

// likePatternNodes unwraps the pgparser representation of an explicit
// ESCAPE clause. The grammar rewrites `value LIKE pattern ESCAPE escape` to
// `value ~~ pg_catalog.like_escape(pattern, escape)`.
func likePatternNodes(n nodes.Node) (pattern nodes.Node, escape nodes.Node, err error) {
	call, ok := n.(*nodes.FuncCall)
	if !ok || funcName(call.Funcname) != "like_escape" {
		return n, nil, nil
	}
	if call.Args == nil || len(call.Args.Items) != 2 {
		return nil, nil, fmt.Errorf("malformed LIKE ESCAPE expression")
	}
	return call.Args.Items[0], call.Args.Items[1], nil
}

// textLiteralOf identifies a lowered text literal, retaining NULL as a
// distinct constant state. Bound SQL parameters lower to literals before
// this point; columns and computed expressions therefore remain unsupported
// as row-dependent patterns.
func textLiteralOf(e lirwire.Expr) (value string, null bool, ok bool) {
	lit, ok := e.ExprUnion.(*lirwire.LiteralExpr)
	if !ok {
		return "", false, false
	}
	text, ok := lit.Value.ValueUnion.(*lirwire.TextValue)
	if !ok {
		return "", false, false
	}
	if text.Value == nil {
		return "", true, true
	}
	return *text.Value, false, true
}

// tokenizeLikePattern converts one PostgreSQL LIKE pattern to canonical LIR
// parts: adjacent literals coalesce and adjacent '%' wildcards collapse.
func tokenizeLikePattern(pattern string, escape rune, escapeEnabled bool) ([]lirwire.TextMatchExprPart, error) {
	parts := make([]lirwire.TextMatchExprPart, 0, 4)
	var literal strings.Builder
	escaped := false
	flushLiteral := func() {
		if literal.Len() == 0 {
			return
		}
		parts = append(parts, lirwire.LiteralPart(literal.String()))
		literal.Reset()
	}

	for _, r := range pattern {
		switch {
		case escaped:
			literal.WriteRune(r)
			escaped = false
		case escapeEnabled && r == escape:
			escaped = true
		case r == '%':
			flushLiteral()
			if len(parts) == 0 || parts[len(parts)-1].TextMatchExprPartType() != "any_many" {
				parts = append(parts, lirwire.AnyManyPart())
			}
		case r == '_':
			return nil, unsupportedf("LIKE pattern with '_' wildcard")
		default:
			literal.WriteRune(r)
		}
	}
	if escaped {
		return nil, fmt.Errorf("LIKE pattern ends with escape character")
	}
	flushLiteral()
	return parts, nil
}
