# Builds a mostly-static Linux binary of the rad demo: SlateDB's native
# library is compiled from source at the pinned tag and statically linked
# into the Go binary; only glibc and friends remain dynamic.
#
#   task docker:build        # passes SLATEDB_TAG derived from go.mod
#   docker run --rm rad-demo
#
# Stage 1 builds libslatedb_uniffi.a and records the exact system libs a
# static link needs (rustc --print native-static-libs), so the Go link flags
# track SlateDB's dependencies automatically across upgrades.

ARG SLATEDB_TAG=bindings/go/v0.14.1

FROM rust:1-bookworm AS native
ARG SLATEDB_TAG
RUN git clone --depth 1 --branch ${SLATEDB_TAG} https://github.com/slatedb/slatedb /slatedb
WORKDIR /slatedb
RUN cargo rustc --release -p slatedb-uniffi -- --print native-static-libs 2>&1 \
      | tee /tmp/build.log \
    && grep -o 'native-static-libs: .*' /tmp/build.log \
      | head -1 \
      | sed 's/native-static-libs: //' > /slatedb/native-static-libs.txt \
    && cat /slatedb/native-static-libs.txt

FROM golang:1.26-bookworm AS build
WORKDIR /src
COPY go.mod go.sum ./
RUN go mod download
COPY . .
COPY --from=native /slatedb/target/release/libslatedb_uniffi.a /native/
COPY --from=native /slatedb/native-static-libs.txt /native/
RUN CGO_ENABLED=1 \
    CGO_LDFLAGS="-L/native $(cat /native/native-static-libs.txt)" \
    go -C demo build -o /out/rad-demo . \
    && ldd /out/rad-demo

FROM debian:bookworm-slim
COPY --from=build /out/rad-demo /usr/local/bin/rad-demo
WORKDIR /data
ENTRYPOINT ["rad-demo"]
