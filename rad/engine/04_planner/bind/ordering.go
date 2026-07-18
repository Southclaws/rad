package bind

import (
	"slices"

	lir "github.com/Southclaws/rad/rad/engine/03_lir"
	"github.com/Southclaws/rad/rad/engine/03_lir/bound"
)

// appendTieBreaker extends an order with a known unique key so tied rows have
// the same order under every access path.
func appendTieBreaker(in bound.Relation, terms []bound.OrderTerm) []bound.OrderTerm {
	key := uniqueKeyFields(in)
	if key == nil {
		return terms
	}
	referenced := map[lir.SlotID]bool{}
	for _, term := range terms {
		if ref, ok := term.Expr.(bound.SlotRef); ok {
			referenced[ref.Slot] = true
		}
	}
	for _, field := range key {
		if !referenced[field.Slot] {
			terms = append(terms, bound.OrderTerm{
				Expr: bound.SlotRef{Slot: field.Slot, Name: field.Name, T: field.Type},
			})
		}
	}
	return terms
}

func uniqueKeyFields(rel bound.Relation) []lir.Field {
	switch rel := rel.(type) {
	case *bound.Scan:
		key := make([]lir.Field, 0, len(rel.Table.PrimaryKey))
		for _, column := range rel.Table.PrimaryKey {
			field, ok := rel.Output().Lookup(column)
			if !ok {
				return nil
			}
			key = append(key, field)
		}
		return key
	case *bound.Filter:
		return uniqueKeyFields(rel.In)
	case *bound.Order:
		return uniqueKeyFields(rel.In)
	case *bound.Slice:
		return uniqueKeyFields(rel.In)
	case *bound.Distinct:
		return rel.Output().Fields
	case *bound.Project:
		key := uniqueKeyFields(rel.In)
		if key == nil {
			return nil
		}
		for _, field := range key {
			if !slices.Contains(rel.Output().Slots(), field.Slot) {
				return nil
			}
		}
		return key
	case *bound.Aggregate:
		if len(rel.Groups) == 0 {
			return nil
		}
		key := make([]lir.Field, 0, len(rel.Groups))
		out := rel.Output()
		for _, group := range rel.Groups {
			field, ok := out.Lookup(group.Name)
			if !ok {
				return nil
			}
			key = append(key, field)
		}
		return key
	}
	return nil
}
