// Package golang is the Go client generator. It emits a single self-contained
// package: typed models, table handles, fluent query builders, includes, and
// aggregates, plus a static query-assembly runtime that compiles a builder's
// accumulated spec into Rad's LIR on the wire (via the lirwire builders). The
// generated client speaks the rad:// protocol through the radclient runtime
// and has no SQL or engine imports.
//
// Unlike the string-concatenation generators, this one authors its output as
// embedded text/template files (see templates/): client.tmpl for the header,
// runtime.tmpl for the verbatim LIR runtime, and table.tmpl for the per-table
// body. The result is gofmt-ed before it is returned.
package golang

import (
	"bytes"
	"embed"
	"fmt"
	"go/format"
	"strings"
	"text/template"

	"github.com/Southclaws/rad/rad/codegen"
)

func init() { codegen.Register("go", generator{}) }

// generator is the registered Go Generator.
type generator struct{}

//go:embed templates/*.tmpl
var templatesFS embed.FS

// tmplData is the template's root context: the model, plus the schema source
// (which lives on Options, not the Model) so the header can embed it.
type tmplData struct {
	*codegen.Model
	SchemaSource string
}

// Generate emits the typed Go client for the model as one gofmt-ed file.
func (generator) Generate(m *codegen.Model, opts codegen.Options) ([]codegen.GeneratedFile, error) {
	tmpl, err := template.New("golang").Funcs(funcMap).ParseFS(templatesFS, "templates/*.tmpl")
	if err != nil {
		return nil, fmt.Errorf("codegen/golang: parse templates (bug): %w", err)
	}

	var b bytes.Buffer
	data := tmplData{Model: m, SchemaSource: string(opts.SchemaSource)}
	if err := tmpl.ExecuteTemplate(&b, "file", data); err != nil {
		return nil, fmt.Errorf("codegen/golang: execute template (bug): %w", err)
	}

	src, err := format.Source(b.Bytes())
	if err != nil {
		return nil, fmt.Errorf("codegen/golang: generated source does not format (bug): %w\n%s", err, b.Bytes())
	}
	return []codegen.GeneratedFile{{Path: opts.Package + ".go", Content: src}}, nil
}

// funcMap holds the small fragment-rendering helpers the templates call. The
// mechanical name mappings defer to the parent codegen package (GoName,
// UqCols); the rest reproduce the imperative helpers the old codegen.go had.
var funcMap = template.FuncMap{
	"goName":            codegen.GoName,
	"uqCols":            codegen.UqCols,
	"lowerFirst":        lowerFirst,
	"jsonOmit":          jsonOmit,
	"goHelper":          goHelper,
	"recHelper":         recHelper,
	"keyType":           keyType,
	"pkParams":          pkParams,
	"pkKeyMap":          pkKeyMap,
	"defaultOrderTerms": defaultOrderTerms,
	"pairsLit":          pairsLit,
	"uqName":            uqName,
	"uqParams":          uqParams,
	"dict":              dict,
}

// dict builds a map from alternating key/value template arguments, the usual
// way to pass a multi-field context to a {{template}} invocation.
func dict(pairs ...any) (map[string]any, error) {
	if len(pairs)%2 != 0 {
		return nil, fmt.Errorf("dict: odd argument count")
	}
	m := make(map[string]any, len(pairs)/2)
	for i := 0; i < len(pairs); i += 2 {
		key, ok := pairs[i].(string)
		if !ok {
			return nil, fmt.Errorf("dict: key %d is not a string", i)
		}
		m[key] = pairs[i+1]
	}
	return m, nil
}

func lowerFirst(s string) string {
	if s == "ID" {
		return "id"
	}
	out := []byte(s)
	out[0] = out[0] - 'A' + 'a'
	return string(out)
}

func jsonOmit(nullable bool) string {
	if nullable {
		return ",omitempty"
	}
	return ""
}

// goHelper maps a base Go type to its record-decode helper suffix.
func goHelper(goType string) string {
	switch goType {
	case "string":
		return "String"
	case "int64":
		return "Int64"
	case "float64":
		return "Float64"
	case "bool":
		return "Bool"
	}
	return "String"
}

// recHelper is the record decoder for a column: rec<Kind>, plus Ptr when the
// column is nullable (a *T field).
func recHelper(c codegen.Col) string {
	h := "rec" + goHelper(c.GoType)
	if c.Nullable {
		h += "Ptr"
	}
	return h
}

// keyType is a column's Go type as a group key: *T when nullable.
func keyType(c codegen.Col) string {
	if c.Nullable {
		return "*" + c.GoType
	}
	return c.GoType
}

// pkParams renders the primary key as Go parameters, e.g.
// "teamID string, userID string".
func pkParams(t *codegen.Table) string {
	out := ""
	for i, c := range t.PK {
		if i > 0 {
			out += ", "
		}
		out += lowerFirst(c.Field) + " " + c.GoType
	}
	return out
}

// pkKeyMap renders the wire key map from PK parameters.
func pkKeyMap(t *codegen.Table) string {
	out := "map[string]any{"
	for i, c := range t.PK {
		if i > 0 {
			out += ", "
		}
		out += fmt.Sprintf("%q: %s", c.Name, lowerFirst(c.Field))
	}
	return out + "}"
}

// defaultOrderTerms renders primary-key ordering for queries that promise one
// deterministic row without accepting an explicit ordering method.
func defaultOrderTerms(t *codegen.Table) string {
	terms := make([]string, len(t.PK))
	for i, c := range t.PK {
		terms[i] = fmt.Sprintf("lirwire.OrderTerm{Expr: lirwire.Col(\"\", %q)}", c.Name)
	}
	return "[]lirwire.OrderTerm{" + strings.Join(terms, ", ") + "}"
}

// pairsLit renders a relation's correlation pairs as a Go literal.
func pairsLit(pairs [][2]string) string {
	out := "[][2]string{"
	for i, pr := range pairs {
		if i > 0 {
			out += ", "
		}
		out += fmt.Sprintf("{%q, %q}", pr[0], pr[1])
	}
	return out + "}"
}

// uqName renders a unique index's lookup method name: ByTeamByUser.
func uqName(uq []codegen.Col) string {
	name := "By"
	for _, c := range uq {
		name += c.Field
	}
	return name
}

// uqParams renders a unique index's lookup parameters: "team string, user string".
func uqParams(uq []codegen.Col) string {
	out := ""
	for i, c := range uq {
		if i > 0 {
			out += ", "
		}
		out += lowerFirst(c.Field) + " " + c.GoType
	}
	return out
}
