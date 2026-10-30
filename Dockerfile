# syntax=docker/dockerfile:1
FROM rust:1.94.1-bookworm AS build
RUN apt-get update && apt-get install -y --no-install-recommends cmake clang && rm -rf /var/lib/apt/lists/*
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY examples ./examples
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/build/target,sharing=locked \
    cargo build --release --locked && cp target/release/inkspect-engine /usr/local/bin/inkspect-engine

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates git \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --uid 10001 --create-home engine \
    && mkdir /reports /workspace && chown engine:engine /reports /workspace
COPY --from=build /usr/local/bin/inkspect-engine /usr/local/bin/inkspect-engine
USER engine
WORKDIR /workspace
ENTRYPOINT ["inkspect-engine"]
CMD ["--help"]
