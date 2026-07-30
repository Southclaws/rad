FROM ubuntu:24.04@sha256:4fbb8e6a8395de5a7550b33509421a2bafbc0aab6c06ba2cef9ebffbc7092d90

RUN apt-get update \
    && apt-get install --yes --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*

COPY rad /usr/local/bin/rad
COPY BUILD.json /usr/share/rad/BUILD.json

ENV RAD_STORAGE=file RAD_DATA_DIR=/data
VOLUME /data
EXPOSE 7237 7238
ENTRYPOINT ["rad"]
CMD ["serve"]
