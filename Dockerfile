# ---------- Build ----------
FROM rust:bookworm AS builder

WORKDIR /app

COPY Cargo.toml Cargo.lock ./
COPY src ./src

RUN cargo build --locked --release


# ---------- Runtime ----------
FROM debian:bookworm-slim

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 --no-create-home shocs

COPY --from=builder \
    /app/target/release/shocs-lc \
    /usr/local/bin/shocs-lc

USER shocs

ENTRYPOINT ["/usr/local/bin/shocs-lc"]