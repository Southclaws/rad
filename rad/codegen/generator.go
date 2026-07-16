package codegen

import "sort"

// A Generator turns the language-agnostic Model into one or more source files
// for a target language. It is the single seam every client generator sits
// behind — built-in (Go, TypeScript) today, or an external `rad-gen-<name>`
// tool satisfying the same contract over a subprocess: the Model serialises
// to JSON in, []GeneratedFile serialises to JSON out, so an in-process
// generator and an external executable are interchangeable.
//
// The Model is the real public contract, not this Go interface — keep it clean
// and versioned as generators multiply.
type Generator interface {
	Generate(m *Model, opts Options) ([]GeneratedFile, error)
}

// GeneratedFile is one output file. Path is relative to the output directory
// and may include directories (e.g. "tasks/where.go") so a generator can emit
// a whole package tree, not just a single file.
type GeneratedFile struct {
	Path    string
	Content []byte
}

// Options are the generic knobs every generator honours. Language-specific
// options are the generator's own concern.
type Options struct {
	// Package is the Go package name (Go) or the file basename (TypeScript).
	Package string
	// SchemaSource is the schema.rad text, embedded verbatim so the generated
	// client can migrate its own database.
	SchemaSource []byte
}

// registry holds the built-in generators, keyed by the --lang name. Each
// generator package registers itself from an init(); the CLI blank-imports
// them so the parent codegen package never imports a generator (no cycle).
var registry = map[string]Generator{}

// Register adds a generator under a language name. It is called from generator
// packages' init functions.
func Register(name string, g Generator) { registry[name] = g }

// Lookup returns the generator registered for a language name.
func Lookup(name string) (Generator, bool) {
	g, ok := registry[name]
	return g, ok
}

// Languages lists the registered language names, sorted.
func Languages() []string {
	names := make([]string, 0, len(registry))
	for name := range registry {
		names = append(names, name)
	}
	sort.Strings(names)
	return names
}
