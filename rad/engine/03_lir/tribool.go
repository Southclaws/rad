package lir

// TriBool is a Kleene K3 truth value. Predicates over nullable operands
// evaluate to tri-bools; Filter keeps only TriTrue. The ordering
// False < Unknown < True makes And a minimum and Or a maximum.
type TriBool int8

const (
	TriFalse TriBool = iota
	TriUnknown
	TriTrue
)

// FromBool lifts a two-valued bool.
func FromBool(b bool) TriBool {
	if b {
		return TriTrue
	}
	return TriFalse
}

// And is Kleene conjunction: False dominates, then Unknown.
func (a TriBool) And(b TriBool) TriBool {
	if b < a {
		return b
	}
	return a
}

// Or is Kleene disjunction: True dominates, then Unknown.
func (a TriBool) Or(b TriBool) TriBool {
	if b > a {
		return b
	}
	return a
}

// Not negates: Unknown stays Unknown.
func (a TriBool) Not() TriBool { return TriTrue - a }

func (a TriBool) String() string {
	switch a {
	case TriTrue:
		return "TRUE"
	case TriFalse:
		return "FALSE"
	default:
		return "UNKNOWN"
	}
}
