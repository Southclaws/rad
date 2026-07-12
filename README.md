# Rad

Rad is a relational database and generated-client toolchain written in Go.
It is designed around a schema-first workflow for building typed applications:

```text
schema.rad → rad migrate → rad generate → application code
```

The repository contains the database server, the `rad` CLI, client libraries,
code generators, and a small end-to-end demo. The project is an active proof
of concept, so interfaces and behavior are still evolving.

## Try the demo

The demo is a small team task tracker using a generated client:

```sh
task demo
```

To keep the server and demo running together:

```sh
task up
```

## Basic workflow

Start a server:

```sh
rad serve
```

Apply a schema to a running server and generate a typed client:

```sh
rad migrate -u rad://localhost -f schema.rad
rad generate -f schema.rad -o ./generated --pkg db
```

The generated client is intended to be the application-facing API. When the
schema changes, run the migration and regenerate the client.

## Development

Useful project commands are defined in [`Taskfile.yml`](Taskfile.yml):

```sh
task test
task build
```

See [`docs/v0-spec.md`](docs/v0-spec.md) for the current proof-of-concept
goals, and [`examples/demo/`](examples/demo/) for a working example.
