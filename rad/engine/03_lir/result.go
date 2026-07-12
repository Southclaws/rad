package lir

// Datum is the typed result value tree a read produces: null, a scalar, an
// object of named datums, or an array. It replaces flat row lists — the
// result is always shaped like the request (a First field is an object or
// null, an Array field an array, a Scalar field a naked value), and the
// frontend renders it to JSON without knowing anything about relations.
//
// Datum is a tagged union in the same style as Value: Kind selects which
// payload is meaningful.
type Datum struct {
	Kind   DatumKind
	Scalar Value         // DatumScalar
	Fields []ObjectField // DatumObject, in output order
	Elems  []Datum       // DatumArray
}

// DatumKind selects a Datum's payload.
type DatumKind uint8

const (
	DatumNull DatumKind = iota
	DatumScalar
	DatumObject
	DatumArray
)

// ObjectField is one named attribute of an object datum. Order is
// deterministic: spread columns first, then computed fields, as projected.
type ObjectField struct {
	Name  string
	Datum Datum
}

// NullDatum is the absent value: a NULL scalar field, an empty First.
func NullDatum() Datum { return Datum{Kind: DatumNull} }

// ScalarDatum wraps a scalar value. A NULL Value becomes a null datum so
// renderers have exactly one representation of absence.
func ScalarDatum(v Value) Datum {
	if v.Null {
		return NullDatum()
	}
	return Datum{Kind: DatumScalar, Scalar: v}
}

// ObjectDatum builds an object from ordered fields.
func ObjectDatum(fields []ObjectField) Datum {
	return Datum{Kind: DatumObject, Fields: fields}
}

// ArrayDatum builds an array. A nil slice is an empty array, never null.
func ArrayDatum(elems []Datum) Datum {
	if elems == nil {
		elems = []Datum{}
	}
	return Datum{Kind: DatumArray, Elems: elems}
}
