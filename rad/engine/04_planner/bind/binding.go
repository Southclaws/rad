package bind

import (
	"fmt"
	"slices"

	lir "github.com/Southclaws/rad/rad/engine/03_lir"
	"github.com/Southclaws/rad/rad/engine/reject"
)

func bindingErr(name string, err error) error {
	return fmt.Errorf("planner: binding %q: %w", name, err)
}

func bindingOrder(bindings map[string]lir.Relation) ([]string, error) {
	names := make([]string, 0, len(bindings))
	for name := range bindings {
		if name == "" {
			return nil, reject.Inputf("planner: binding names must not be empty")
		}
		names = append(names, name)
	}
	slices.Sort(names)

	const (
		visiting = 1
		visited  = 2
	)
	state := map[string]uint8{}
	var order []string
	var visit func(string) error
	visit = func(name string) error {
		if _, ok := bindings[name]; !ok {
			return nil
		}
		switch state[name] {
		case visiting:
			return reject.Inputf("planner: binding %q is part of a binding cycle", name)
		case visited:
			return nil
		}
		state[name] = visiting
		for _, dependency := range lirBindingDeps(bindings[name]) {
			if err := visit(dependency); err != nil {
				return err
			}
		}
		state[name] = visited
		order = append(order, name)
		return nil
	}
	for _, name := range names {
		if err := visit(name); err != nil {
			return nil, err
		}
	}
	return order, nil
}

func lirBindingDeps(relation lir.Relation) []string {
	var dependencies []string
	_ = walkLIR(relation, func(relation lir.Relation) error {
		if ref, ok := relation.(lir.Ref); ok {
			dependencies = append(dependencies, ref.Binding)
		}
		return nil
	}, nil)
	return dependencies
}
