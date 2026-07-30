# Rad's HTTP Interface

Rad uses HTTP as its communication surface. We're not married to it yet, but it
offers a simple, easily debuggable, plaintext-friendly API/SDK interface for
the proof of concept. It's a minimal API that permits execution of PIR programs,
as well as simple CRUD operations for schema changes (while Rad is in Direct
schema mode) or managed schema migration (while Rad is in Schema mode.)

This drives the server interface generation for the Rad server, and also drives
client generation for the supported languages (./clients etc.)
