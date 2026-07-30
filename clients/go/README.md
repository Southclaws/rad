# Rad client for Go

This directory is the independently versioned Go client module. It is a
container, not an importable Go package.

Applications use the runtime package:

```go
import "github.com/Southclaws/rad/clients/go/rad"

client, err := rad.Dial("rad://localhost")
```

The sibling `protocol` packages contain Schemancer-generated LIR/PIR wire
models and thin handwritten builders. Schema-specific clients emitted by
`rad generate --lang go` import these shared packages; they never generate
private copies of the wire contracts.

```text
clients/go/
  rad/                 HTTP runtime and ergonomic data/catalog operations
  protocol/            shared protocol values and validation
    lirwire/            Schemancer output plus LIR builders
    pirwire/            Schemancer output plus PIR builders
  api/                 generated OpenAPI bindings and boundary conversion
```

Run `task generate:protocol` from the repository root after changing the
normative schemas in `protocol/`. It regenerates both the canonical Rust types
and the Go files in this module.
