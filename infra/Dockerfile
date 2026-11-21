# syntax=docker/dockerfile:1
# Multi-stage standalone build for `reticula-server` and the precompressed web bundle.

FROM node:26-bookworm-slim AS node-dist

FROM rust:1.100-bookworm AS builder
COPY --from=node-dist /usr/local/bin/node /usr/local/bin/node
COPY --from=node-dist /usr/local/lib/node_modules /usr/local/lib/node_modules
RUN ln -s /usr/local/lib/node_modules/npm/bin/npm-cli.js /usr/local/bin/npm \
    && ln -s /usr/local/lib/node_modules/npm/bin/npx-cli.js /usr/local/bin/npx \
    && rustup target add wasm32-unknown-unknown

WORKDIR /src
COPY package.json pnpm-lock.yaml pnpm-workspace.yaml tsconfig.base.json biome.json ./
RUN --mount=type=cache,target=/root/.cache/pnpm \
    npm install --global "$(node --print "require('./package.json').packageManager")" \
    && pnpm config set store-dir /root/.cache/pnpm/store \
    && pnpm install --frozen-lockfile

COPY Cargo.toml Cargo.lock rust-toolchain.toml deny.toml ./
COPY .cargo ./.cargo
COPY crates ./crates
COPY spikes ./spikes
COPY bench ./bench
COPY packages ./packages
COPY apps ./apps
COPY tests ./tests
COPY playwright.config.ts ./

# The caches keep dependency builds between local image builds; the server
# binary is copied out of the cached target directory to reach the next stage.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    --mount=type=cache,target=/src/.tools \
    cargo install --locked --root .tools --version 0.2.130 wasm-bindgen-cli \
    && cargo xtask build --check-budget \
    && env SQLX_OFFLINE=true cargo build --locked --release --package reticula-server \
    && cp target/release/reticula-server /usr/local/bin/reticula-server

FROM debian:bookworm-slim AS runtime
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --uid 10001 --user-group --no-create-home --shell /usr/sbin/nologin reticula \
    && mkdir -p /var/lib/reticula \
    && chown 10001:10001 /var/lib/reticula

WORKDIR /srv/reticula
COPY --from=builder /usr/local/bin/reticula-server /usr/local/bin/reticula-server
COPY --from=builder /src/apps/web/dist ./dist

USER 10001:10001
ENV BIND_ADDR=0.0.0.0:3200
ENV RETICULA_STATIC_DIR=/srv/reticula/dist
ENV RETICULA_DATA_DIR=/var/lib/reticula
EXPOSE 3200

ENTRYPOINT ["/usr/local/bin/reticula-server"]
