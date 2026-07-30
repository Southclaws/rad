# Builds the Rad server image from the canonical Cargo package. Configure
# storage with RAD_STORAGE / RAD_DATA_DIR / RAD_S3_*. Rad speaks plain HTTP.
# Publish both 7237 (public API) and 7238 (administration UI).
FROM rust:1.96.0-bookworm AS build
WORKDIR /src
COPY . .
RUN cargo build --locked --release \
    && install -D target/release/rad /out/rad \
    && ldd /out/rad

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install --yes --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=build /out/rad /usr/local/bin/rad
ENV RAD_STORAGE=file RAD_DATA_DIR=/data
VOLUME /data
EXPOSE 7237 7238
ENTRYPOINT ["rad"]
CMD ["serve"]
