# Builds the RAD server image: one mostly-static binary (SlateDB compiled
# from source at the go.mod-pinned tag and statically linked; only glibc
# stays dynamic). Configure storage with RAD_STORAGE / RAD_DATA_DIR /
# RAD_S3_* and put a TLS-terminating reverse proxy in front for rads://.
#
#   task docker:build
#   docker run --rm -p 7237:7237 -e RAD_STORAGE=memory rad
#   docker run --rm -p 7237:7237 -v raddata:/data rad          # file storage
#   docker run --rm -p 7237:7237 -e RAD_STORAGE=s3 -e RAD_S3_BUCKET=... rad

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
    go build -trimpath -ldflags "-s -w" -o /out/rad ./cmd/rad \
    && ldd /out/rad

FROM debian:bookworm-slim
COPY --from=build /out/rad /usr/local/bin/rad
ENV RAD_STORAGE=file RAD_DATA_DIR=/data
VOLUME /data
EXPOSE 7237
ENTRYPOINT ["rad"]
CMD ["serve"]
