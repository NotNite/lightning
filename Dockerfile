ARG RUST_VERSION=1.83-bookworm
ARG APP_NAME=lightning

FROM rust:${RUST_VERSION} AS build
ARG APP_NAME
WORKDIR /app

RUN --mount=type=bind,source=src,target=src \
    --mount=type=bind,source=Cargo.toml,target=Cargo.toml \
    --mount=type=bind,source=Cargo.lock,target=Cargo.lock \
    --mount=type=cache,target=/app/target/ \
    --mount=type=cache,target=/usr/local/cargo/git/db \
    --mount=type=cache,target=/usr/local/cargo/registry/ \
    cargo build --locked --release && \
    cp ./target/release/$APP_NAME /bin/server

FROM debian:bookworm AS final
RUN apt-get update \
    && apt-get install -y libssl3 \
    && rm -rf /var/lib/apt/lists/*
COPY --from=build /bin/server /bin/
CMD ["/bin/server", "/app/config.json"]
