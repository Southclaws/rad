package sql

import (
	"reflect"
	"strings"
	"testing"

	"github.com/Southclaws/rad/rad/protocol"
	"github.com/Southclaws/rad/rad/protocol/lirwire"
	"github.com/Southclaws/rad/rad/protocol/pirwire"
)

func TestTokenizeLikePattern(t *testing.T) {
	tests := []struct {
		name          string
		pattern       string
		escape        rune
		escapeEnabled bool
		want          []string
		wantErr       string
	}{
		{name: "literal", pattern: "foo", escape: '\\', escapeEnabled: true, want: []string{"lit:foo"}},
		{name: "canonical wildcards", pattern: "%%foo%%%bar%%", escape: '\\', escapeEnabled: true, want: []string{"any", "lit:foo", "any", "lit:bar", "any"}},
		{name: "escaped wildcard and escape", pattern: `\%\_\\`, escape: '\\', escapeEnabled: true, want: []string{`lit:%_\`}},
		{name: "custom escape", pattern: "100#%%", escape: '#', escapeEnabled: true, want: []string{"lit:100%", "any"}},
		{name: "Unicode escape", pattern: "left§%right%", escape: '§', escapeEnabled: true, want: []string{"lit:left%right", "any"}},
		{name: "escape disabled", pattern: `a\%`, escape: '\\', escapeEnabled: false, want: []string{`lit:a\`, "any"}},
		{name: "empty", pattern: "", escape: '\\', escapeEnabled: true, want: nil},
		{name: "underscore", pattern: "a_b", escape: '\\', escapeEnabled: true, wantErr: "'_' wildcard"},
		{name: "dangling escape", pattern: "abc#", escape: '#', escapeEnabled: true, wantErr: "ends with escape character"},
	}

	for _, tc := range tests {
		t.Run(tc.name, func(t *testing.T) {
			parts, err := tokenizeLikePattern(tc.pattern, tc.escape, tc.escapeEnabled)
			if tc.wantErr != "" {
				if err == nil || !strings.Contains(err.Error(), tc.wantErr) {
					t.Fatalf("error = %v, want substring %q", err, tc.wantErr)
				}
				return
			}
			if err != nil {
				t.Fatal(err)
			}
			if got := describeLikeParts(parts); !reflect.DeepEqual(got, tc.want) {
				t.Fatalf("parts = %v, want %v", got, tc.want)
			}
		})
	}
}

func TestLowerLikeAndILikeToTextMatch(t *testing.T) {
	tests := []struct {
		name       string
		sql        string
		args       []lirwire.Value
		comparison lirwire.TextComparison
		parts      []string
		negated    bool
		paramCount int
	}{
		{
			name:       "LIKE literal and wildcard",
			sql:        `SELECT id FROM samples WHERE value LIKE 'foo%bar%%'`,
			comparison: lirwire.TextComparisonExact,
			parts:      []string{"lit:foo", "any", "lit:bar", "any"},
		},
		{
			name:       "ILIKE becomes Unicode simple fold",
			sql:        `SELECT id FROM samples WHERE value ILIKE '%FoO%'`,
			comparison: lirwire.TextComparisonUnicodeSimpleFold,
			parts:      []string{"any", "lit:FoO", "any"},
		},
		{
			name:       "NOT ILIKE preserves K3 negation",
			sql:        `SELECT id FROM samples WHERE value NOT ILIKE 'foo%'`,
			comparison: lirwire.TextComparisonUnicodeSimpleFold,
			parts:      []string{"lit:foo", "any"},
			negated:    true,
		},
		{
			name:       "NOT LIKE uses exact comparison",
			sql:        `SELECT id FROM samples WHERE value NOT LIKE '%Foo'`,
			comparison: lirwire.TextComparisonExact,
			parts:      []string{"any", "lit:Foo"},
			negated:    true,
		},
		{
			name:       "custom ESCAPE makes percent literal",
			sql:        `SELECT id FROM samples WHERE value LIKE '100#%%' ESCAPE '#'`,
			comparison: lirwire.TextComparisonExact,
			parts:      []string{"lit:100%", "any"},
		},
		{
			name:       "escaped underscore is literal",
			sql:        `SELECT id FROM samples WHERE value ILIKE 'a#_b' ESCAPE '#'`,
			comparison: lirwire.TextComparisonUnicodeSimpleFold,
			parts:      []string{"lit:a_b"},
		},
		{
			name:       "empty ESCAPE disables escaping",
			sql:        `SELECT id FROM samples WHERE value LIKE 'a#%' ESCAPE ''`,
			comparison: lirwire.TextComparisonExact,
			parts:      []string{"lit:a#", "any"},
		},
		{
			name:       "bound pattern parameter",
			sql:        `SELECT id FROM samples WHERE value ILIKE $1`,
			args:       []lirwire.Value{lirwire.Text(`%Needle%`)},
			comparison: lirwire.TextComparisonUnicodeSimpleFold,
			parts:      []string{"any", "lit:Needle", "any"},
			paramCount: 1,
		},
		{
			name:       "bound pattern and escape parameters",
			sql:        `SELECT id FROM samples WHERE value ILIKE $1 ESCAPE $2`,
			args:       []lirwire.Value{lirwire.Text(`100!%`), lirwire.Text(`!`)},
			comparison: lirwire.TextComparisonUnicodeSimpleFold,
			parts:      []string{"lit:100%"},
			paramCount: 2,
		},
	}

	for _, tc := range tests {
		t.Run(tc.name, func(t *testing.T) {
			predicate, prepared, err := compileLikePredicate(tc.sql, tc.args)
			if err != nil {
				t.Fatal(err)
			}
			if tc.paramCount > 0 {
				if len(prepared.Params) != tc.paramCount {
					t.Fatalf("parameter types = %#v, want %d text parameters", prepared.Params, tc.paramCount)
				}
				for _, param := range prepared.Params {
					if param.Scalar != lirwire.ScalarTypeText {
						t.Fatalf("parameter types = %#v, want text parameters", prepared.Params)
					}
				}
			}
			if tc.negated {
				not, ok := predicate.ExprUnion.(*lirwire.UnaryExpr)
				if !ok || not.Op != "not" {
					t.Fatalf("predicate = %#v, want unary not", predicate.ExprUnion)
				}
				predicate = not.Expr
			}
			match, ok := predicate.ExprUnion.(*lirwire.TextMatchExpr)
			if !ok {
				t.Fatalf("predicate = %T, want TextMatchExpr", predicate.ExprUnion)
			}
			gotComparison := lirwire.TextComparisonExact
			if match.Comparison != nil {
				gotComparison = *match.Comparison
			}
			if gotComparison != tc.comparison {
				t.Fatalf("comparison = %q, want %q", gotComparison, tc.comparison)
			}
			if got := describeLikeParts(match.Parts); !reflect.DeepEqual(got, tc.parts) {
				t.Fatalf("parts = %v, want %v", got, tc.parts)
			}
		})
	}
}

func TestLowerLikeSpecialAndRejectedPatterns(t *testing.T) {
	t.Run("empty pattern is exact empty equality", func(t *testing.T) {
		predicate, _, err := compileLikePredicate(`SELECT id FROM samples WHERE value ILIKE ''`, nil)
		if err != nil {
			t.Fatal(err)
		}
		binary, ok := predicate.ExprUnion.(*lirwire.BinaryExpr)
		if !ok || binary.Op != "eq" {
			t.Fatalf("predicate = %#v, want empty equality", predicate.ExprUnion)
		}
	})

	t.Run("NULL pattern is UNKNOWN", func(t *testing.T) {
		predicate, _, err := compileLikePredicate(`SELECT id FROM samples WHERE value LIKE NULL`, nil)
		if err != nil {
			t.Fatal(err)
		}
		literal, ok := predicate.ExprUnion.(*lirwire.LiteralExpr)
		if !ok {
			t.Fatalf("predicate = %T, want NULL bool literal", predicate.ExprUnion)
		}
		value, ok := literal.Value.ValueUnion.(*lirwire.BoolValue)
		if !ok || value.Value != nil {
			t.Fatalf("predicate literal = %#v, want NULL bool", literal.Value.ValueUnion)
		}
	})

	t.Run("NULL escape is UNKNOWN", func(t *testing.T) {
		predicate, _, err := compileLikePredicate(`SELECT id FROM samples WHERE value NOT ILIKE 'foo%' ESCAPE NULL`, nil)
		if err != nil {
			t.Fatal(err)
		}
		literal, ok := predicate.ExprUnion.(*lirwire.LiteralExpr)
		if !ok {
			t.Fatalf("predicate = %T, want NULL bool literal", predicate.ExprUnion)
		}
		value, ok := literal.Value.ValueUnion.(*lirwire.BoolValue)
		if !ok || value.Value != nil {
			t.Fatalf("predicate literal = %#v, want NULL bool", literal.Value.ValueUnion)
		}
	})

	for _, tc := range []struct {
		name string
		sql  string
		want string
	}{
		{name: "unescaped underscore", sql: `SELECT id FROM samples WHERE value LIKE 'a_b'`, want: "'_' wildcard"},
		{name: "dangling escape", sql: `SELECT id FROM samples WHERE value LIKE 'abc#' ESCAPE '#'`, want: "ends with escape character"},
		{name: "multi-character escape", sql: `SELECT id FROM samples WHERE value LIKE 'abc' ESCAPE '##'`, want: "empty or one character"},
		{name: "row-dependent pattern", sql: `SELECT id FROM samples WHERE value LIKE pattern`, want: "non-constant LIKE pattern"},
		{name: "row-dependent escape", sql: `SELECT id FROM samples WHERE value LIKE 'abc' ESCAPE pattern`, want: "non-constant LIKE escape"},
	} {
		t.Run(tc.name, func(t *testing.T) {
			_, _, err := compileLikePredicate(tc.sql, nil)
			if err == nil || !strings.Contains(err.Error(), tc.want) {
				t.Fatalf("error = %v, want substring %q", err, tc.want)
			}
		})
	}
}

func compileLikePredicate(sqlText string, args []lirwire.Value) (lirwire.Expr, *Prepared, error) {
	schema, err := NewSchema([]protocol.TableInfo{{
		ID:   1,
		Name: "samples",
		Columns: []protocol.ColumnInfo{
			{ID: 1, Name: "id", Type: "text"},
			{ID: 2, Name: "value", Type: "text", Nullable: true},
			{ID: 3, Name: "pattern", Type: "text"},
		},
		PrimaryKey: []string{"id"},
	}})
	if err != nil {
		return lirwire.Expr{}, nil, err
	}
	statements, err := Parse(sqlText)
	if err != nil {
		return lirwire.Expr{}, nil, err
	}
	prepared, err := Prepare(schema, statements[0])
	if err != nil {
		return lirwire.Expr{}, nil, err
	}
	compiled, err := prepared.Compile(args)
	if err != nil {
		return lirwire.Expr{}, prepared, err
	}
	queryStmt := compiled.Program.Statements[0].StatementUnion.(*pirwire.QueryStatement)
	query, err := protocol.UnmarshalQuery(queryStmt.Relation)
	if err != nil {
		return lirwire.Expr{}, prepared, err
	}
	for _, node := range query.Nodes {
		if filter, ok := node.NodeUnion.(*lirwire.FilterNode); ok {
			return filter.Predicate, prepared, nil
		}
	}
	return lirwire.Expr{}, prepared, nil
}

func describeLikeParts(parts []lirwire.TextMatchExprPart) []string {
	if len(parts) == 0 {
		return nil
	}
	out := make([]string, len(parts))
	for i, part := range parts {
		switch value := part.TextMatchExprPartUnion.(type) {
		case *lirwire.LiteralTextMatchPart:
			out[i] = "lit:" + value.Value
		case *lirwire.AnyManyTextMatchPart:
			out[i] = "any"
		default:
			out[i] = "unknown"
		}
	}
	return out
}
