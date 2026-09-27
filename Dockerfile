# syntax=docker/dockerfile:1.7

FROM debian:bookworm-slim@sha256:3783cc01769c7b2b1b83a5c5ad96c815348e28ed7da68e2e3687004faa906251 AS tools

ARG YTDLP_REPOSITORY=yt-dlp/yt-dlp-nightly-builds
ARG YTDLP_VERSION=2026.07.23.234303
ARG DENO_VERSION=2.9.4
ARG DENO_SHA256=c24f955d9fbfe0ea5ae2b501c8e71ae76e31e4c9782390a54a284b3364fda725
ARG YTDLP_KEY_FINGERPRINT=AC0CBBE6848D6A873464AF4E57CF65933B5A7581

RUN apt-get update \
    && apt-get install --no-install-recommends --yes \
        ca-certificates curl gnupg unzip \
    && rm -rf /var/lib/apt/lists/*

COPY deploy/third-party-versions.env /tmp/third-party-versions.env
COPY deploy/yt-dlp-public.key /tmp/yt-dlp-public.key

RUN set -eux; \
    test "$(awk -F= '$1 == "YTDLP_REPOSITORY" {print $2}' /tmp/third-party-versions.env)" = "$YTDLP_REPOSITORY"; \
    test "$(awk -F= '$1 == "YTDLP_VERSION" {print $2}' /tmp/third-party-versions.env)" = "$YTDLP_VERSION"; \
    test "$(awk -F= '$1 == "DENO_VERSION" {print $2}' /tmp/third-party-versions.env)" = "$DENO_VERSION"; \
    test "$(awk -F= '$1 == "DENO_X86_64_LINUX_GNU_SHA256" {print $2}' /tmp/third-party-versions.env)" = "$DENO_SHA256"; \
    mkdir -p /tmp/gnupg /out; chmod 700 /tmp/gnupg; \
    gpg --batch --homedir /tmp/gnupg --import /tmp/yt-dlp-public.key; \
    test "$(gpg --batch --homedir /tmp/gnupg --with-colons --fingerprint | awk -F: '$1 == "fpr" {print $10; exit}')" = "$YTDLP_KEY_FINGERPRINT"; \
    curl --fail --silent --show-error --location --retry 4 --retry-all-errors --retry-delay 2 \
        --connect-timeout 10 --max-time 180 \
        "https://github.com/$YTDLP_REPOSITORY/releases/download/$YTDLP_VERSION/yt-dlp_linux" \
        --output /tmp/yt-dlp; \
    curl --fail --silent --show-error --location --retry 4 --retry-all-errors --retry-delay 2 \
        --connect-timeout 10 --max-time 180 \
        "https://github.com/$YTDLP_REPOSITORY/releases/download/$YTDLP_VERSION/SHA2-256SUMS" \
        --output /tmp/SHA2-256SUMS; \
    curl --fail --silent --show-error --location --retry 4 --retry-all-errors --retry-delay 2 \
        --connect-timeout 10 --max-time 180 \
        "https://github.com/$YTDLP_REPOSITORY/releases/download/$YTDLP_VERSION/SHA2-256SUMS.sig" \
        --output /tmp/SHA2-256SUMS.sig; \
    gpg --batch --homedir /tmp/gnupg --status-fd 1 --verify /tmp/SHA2-256SUMS.sig /tmp/SHA2-256SUMS \
        > /tmp/verify-status; \
    test "$(awk '$1 == "[GNUPG:]" && $2 == "VALIDSIG" {print (NF >= 12 ? $12 : $3); exit}' /tmp/verify-status)" = "$YTDLP_KEY_FINGERPRINT"; \
    test "$(sha256sum /tmp/yt-dlp | awk '{print $1}')" = "$(awk '$2 == "yt-dlp_linux" {print $1; exit}' /tmp/SHA2-256SUMS)"; \
    curl --fail --silent --show-error --location --retry 4 --retry-all-errors --retry-delay 2 \
        --connect-timeout 10 --max-time 180 \
        "https://github.com/denoland/deno/releases/download/v$DENO_VERSION/deno-x86_64-unknown-linux-gnu.zip" \
        --output /tmp/deno.zip; \
    test "$(sha256sum /tmp/deno.zip | awk '{print $1}')" = "$DENO_SHA256"; \
    unzip -q /tmp/deno.zip -d /tmp/deno; \
    test -x /tmp/deno/deno; \
    install -m 0755 /tmp/yt-dlp /out/yt-dlp-fallback; \
    install -m 0755 /tmp/deno/deno /out/deno; \
    /out/yt-dlp-fallback --ignore-config --version | grep -Fx "$YTDLP_VERSION"; \
    /out/deno --version | awk -v expected="$DENO_VERSION" '$1 == "deno" && $2 == expected {found=1} END {exit !found}'; \
    rm -rf /tmp/gnupg /tmp/deno /tmp/yt-dlp /tmp/deno.zip /tmp/SHA2-256SUMS /tmp/SHA2-256SUMS.sig /tmp/verify-status

FROM rust:1.95-bookworm@sha256:6258907abe69656e41cd992e0b705cdcfabcbbe3db374f92ed2d47121282d4a1 AS builder

WORKDIR /src
RUN apt-get update \
    && apt-get install --no-install-recommends --yes build-essential cmake pkg-config \
    && rm -rf /var/lib/apt/lists/*
COPY . .
RUN cargo build --locked --release --package wotoha-app --bin wotoha-app

FROM debian:bookworm-slim@sha256:3783cc01769c7b2b1b83a5c5ad96c815348e28ed7da68e2e3687004faa906251 AS runtime

ARG SOURCE_COMMIT=unknown
ARG IMAGE_VERSION=development

RUN apt-get update \
    && apt-get install --no-install-recommends --yes ca-certificates \
    && rm -rf /var/lib/apt/lists/*

RUN groupadd --system --gid 10001 wotoha \
    && useradd --system --uid 10001 --gid 10001 --home-dir /data --shell /usr/sbin/nologin wotoha \
    && install -d -o 10001 -g 10001 -m 0755 /app /app/tools /app/licenses /data /data/cache/analysis /data/logs /data/tools /tmp

COPY --from=builder /src/target/release/wotoha-app /app/wotoha-app
COPY --from=tools /out/yt-dlp-fallback /app/tools/yt-dlp-fallback
COPY --from=tools /out/deno /app/tools/deno
COPY LICENSE THIRD_PARTY_NOTICES.md /app/licenses/

RUN chmod 0555 /app /app/wotoha-app /app/tools /app/tools/yt-dlp-fallback /app/tools/deno /app/licenses \
    && chmod 0755 /data /data/cache /data/cache/analysis /data/logs /data/tools /tmp \
    && chown -R 10001:10001 /data

ENV WOTOHA_ANALYSIS_CACHE_DIR=/data/cache/analysis \
    WOTOHA_LOG_DIR=/data/logs \
    WOTOHA_DENO_PATH=/app/tools/deno \
    RUST_LOG=info,wotoha_debug=info

LABEL org.opencontainers.image.title="wotoha-rs" \
      org.opencontainers.image.description="Wotoha Discord music bot" \
      org.opencontainers.image.source="https://github.com/NcaMoq/wotoha-rs" \
      org.opencontainers.image.revision="$SOURCE_COMMIT" \
      org.opencontainers.image.version="$IMAGE_VERSION" \
      org.opencontainers.image.licenses="MIT"

WORKDIR /app
USER 10001:10001
VOLUME ["/data"]
ENTRYPOINT ["/app/wotoha-app"]
