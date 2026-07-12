package catalog

// Type is the set of column types the schema layer supports. It is the
// shared type vocabulary referenced upward by the IR (03_lir) and the
// executor (05_exec).
type Type string

const (
	TypeText    Type = "text"
	TypeInt64   Type = "int64"
	TypeFloat64 Type = "float64"
	TypeBool    Type = "bool"
)

// Default is a column default, applied when an insert omits the column:
// either a builtin generator Func or a literal of the column's type.
type Default struct {
	Func    DefaultFunc `json:"func,omitempty"`
	Text    string      `json:"text,omitempty"`
	Int64   int64       `json:"int64,omitempty"`
	Float64 float64     `json:"float64,omitempty"`
	Bool    bool        `json:"bool,omitempty"`
}

// DefaultFunc names a builtin default generator.
type DefaultFunc string

const (
	// DefaultUUID generates a random UUIDv4 string. Text columns only.
	DefaultUUID DefaultFunc = "uuid"
	// DefaultNowMS is the current Unix time in milliseconds. Int64 only.
	DefaultNowMS DefaultFunc = "now_ms"
)
