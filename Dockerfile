# barwatchd: the server that hosts replays. Engines are downloaded at run time into
# the data volume, so the image holds only the daemon.
FROM rust:1-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
RUN cargo build --release --locked -p barwatchd

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates p7zip-full \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --uid 1000 --create-home barwatch \
    && mkdir /data && chown barwatch /data
COPY --from=build /src/target/release/barwatchd /usr/local/bin/barwatchd
USER barwatch
ENV BARWATCH_DATA=/data \
    BARWATCH_LISTEN=0.0.0.0:8780 \
    BARWATCH_UDP_PORTS=8480-8489
VOLUME /data
EXPOSE 8780/tcp 8480-8489/udp
ENTRYPOINT ["/usr/local/bin/barwatchd"]
