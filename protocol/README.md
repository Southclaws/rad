# Protocol schemas

`lir.schema.yaml` and `pir.schema.yaml` are the authored sources of truth for
Rad's wire protocols. Schemancer generates their Rust representations into
`src/protocol/generated`; those files are checked in and must not be edited by
hand.

The schemas also drive cross-language codegen for official clients.
