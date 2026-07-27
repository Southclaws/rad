# Schema migration battle tests

This package exercises desired-schema changes from outside a running Rad
server. Tests use the real Go client over HTTP, a real in-process server, and
an in-memory SlateDB. They do not invoke the CLI: the CLI is one frontend over
the same schema endpoints and has its own focused command tests.

The suite owns cross-layer behaviours that are hard to prove in one package:

- preflight is observational and a rejected apply is atomic;
- dependent online work is exposed as one durable transition graph;
- application writes can cross physical publication safely;
- competing schema applies have a single serializable winner;
- cancelled work is terminal and a later apply starts fresh identities;
- repeated concurrent desired-schema requests converge while independent
  clients blitz the database with creates, updates, point reads, and scans.

Server construction is shared with `tests/harness`; this package adds only
schema-specific request and assertion helpers. Deterministic catalog,
executor, codec, and worker state-machine invariants remain beside their
implementations under `rad/engine`.

Run this package with the repository's SlateDB linker flags:

```sh
CGO_LDFLAGS="-L$PWD/lib" \
  go test -ldflags="-extldflags=-Wl,-rpath,$PWD/lib" ./tests/schema
```
