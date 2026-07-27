package config

type RadProjectConfigurationGenerateItem struct {
	// Target language for code generation. "ts" and "typescript" are
	// aliases for TypeScript generation.
	//
	Language *string `json:"language,omitempty"`
	// Output directory for generated code, relative to the project root.
	//
	Output *string `json:"output,omitempty"`
	// Package or client name for generated code.
	//
	Package *string `json:"package,omitempty"`
}

// The rad.config.yaml file defines the local project configuration: database
// connection URL and code generation targets. It lives in the project root
// alongside rad.schema.yaml.
type RadProjectConfiguration struct {
	// The Rad database connection URL. Format: rad://host[:port] or
	// rads://host[:port] for TLS. The rad:// scheme resolves to plain HTTP,
	// rads:// to HTTPS (Rad never terminates TLS itself; front it with a proxy
	// in production). Port defaults to 7237 if omitted.
	//
	DatabaseURL string `json:"database_url"`
	// Code generation targets. Each entry specifies a language, output
	// directory, and package name. If omitted, defaults to a single Go
	// generator with output="generated" and package="db".
	//
	Generate []RadProjectConfigurationGenerateItem `json:"generate,omitempty"`
}
