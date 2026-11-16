# syntax=docker/dockerfile:1
# Headless Selenite server: MCP over Streamable HTTP, optional background sync.

FROM rust:1-bookworm AS builder
WORKDIR /src
COPY . .
RUN cargo build --release -p selenite-server

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --create-home --uid 10001 selenite \
    && mkdir /vault \
    && chown selenite:selenite /vault
COPY --from=builder /src/target/release/selenite-server /usr/local/bin/selenite-server
USER selenite
# The vault lives on a volume. Set SELENITE_SERVER_TOKEN (16+ characters) at run time.
VOLUME /vault
EXPOSE 8787
# Binds 0.0.0.0 so the port can be published. The server speaks plain HTTP: it MUST sit behind
# a TLS-terminating reverse proxy (see docs/server.md). Add --allowed-host / --allowed-origin
# arguments to restrict the Host and Origin headers to your public name.
ENTRYPOINT ["selenite-server"]
CMD ["--vault", "/vault", "--listen", "0.0.0.0:8787"]
