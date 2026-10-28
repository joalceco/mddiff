# syntax=docker/dockerfile:1.7
FROM rust:1.95.0-bookworm@sha256:6258907abe69656e41cd992e0b705cdcfabcbbe3db374f92ed2d47121282d4a1 AS build
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY src/ src/
COPY web/ web/
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/build/target,sharing=locked \
    cargo build --release --locked --jobs 1 \
    && install -D -m 0755 target/release/mddiff /out/mddiff

FROM debian:bookworm-slim@sha256:99c0b25c46d0214d5a2091092e7908907dcaecd6a962cde0ac700acee222b6f3
RUN apt-get update \
    && apt-get install --no-install-recommends -y ca-certificates curl \
    && rm -rf /var/lib/apt/lists/* \
    && install -d -o 10001 -g 10001 -m 0700 /var/lib/mddiff \
    && install -d -m 0755 /run/secrets/mddiff
COPY --from=build /out/mddiff /usr/local/bin/mddiff
LABEL org.opencontainers.image.source="https://github.com/joalceco/mddiff"
USER 10001:10001
WORKDIR /var/lib/mddiff
EXPOSE 3847
STOPSIGNAL SIGTERM
HEALTHCHECK --interval=20s --timeout=4s --start-period=30s --retries=3 \
    CMD curl --noproxy '*' --fail --silent --show-error --connect-timeout 2 --max-time 3 http://127.0.0.1:3847/healthz || exit 1
ENTRYPOINT ["/usr/local/bin/mddiff"]
CMD ["--mode", "hosted", "--bind", "0.0.0.0", "--port", "3847", "--data-dir", "/var/lib/mddiff", "--oidc-config", "/run/secrets/mddiff/oidc.json", "--connections-config", "/run/secrets/mddiff/connections.json"]
