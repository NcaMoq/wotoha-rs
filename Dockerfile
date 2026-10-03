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
    mkdir -p /out/licenses/yt-dlp /out/licenses/deno; \
    curl --fail --silent --show-error --location --retry 4 --retry-all-errors --retry-delay 2 \
        --connect-timeout 10 --max-time 60 \
        "https://raw.githubusercontent.com/yt-dlp/yt-dlp/master/LICENSE" \
        --output /out/licenses/yt-dlp/LICENSE; \
    curl --fail --silent --show-error --location --retry 4 --retry-all-errors --retry-delay 2 \
        --connect-timeout 10 --max-time 60 \
        "https://raw.githubusercontent.com/denoland/deno/v$DENO_VERSION/LICENSE.md" \
        --output /out/licenses/deno/LICENSE.md; \
    test -s /out/licenses/yt-dlp/LICENSE; \
    test -s /out/licenses/deno/LICENSE.md; \
    printf 'repository=%s\nversion=%s\nrelease_artifact=https://github.com/%s/releases/download/%s/yt-dlp_linux\nlicense_source=https://raw.githubusercontent.com/yt-dlp/yt-dlp/master/LICENSE\n' \
        "$YTDLP_REPOSITORY" "$YTDLP_VERSION" "$YTDLP_REPOSITORY" "$YTDLP_VERSION" \
        > /out/licenses/yt-dlp/PROVENANCE.txt; \
    printf 'repository=denoland/deno\nversion=%s\nrelease_artifact=https://github.com/denoland/deno/releases/download/v%s/deno-x86_64-unknown-linux-gnu.zip\nlicense_source=https://raw.githubusercontent.com/denoland/deno/v%s/LICENSE.md\nsha256=%s\n' \
        "$DENO_VERSION" "$DENO_VERSION" "$DENO_VERSION" "$DENO_SHA256" \
        > /out/licenses/deno/PROVENANCE.txt; \
    /out/yt-dlp-fallback --ignore-config --version | grep -Fx "$YTDLP_VERSION"; \
    /out/deno --version | awk -v expected="$DENO_VERSION" '$1 == "deno" && $2 == expected {found=1} END {exit !found}'; \
    rm -rf /tmp/gnupg /tmp/deno /tmp/yt-dlp /tmp/deno.zip /tmp/SHA2-256SUMS /tmp/SHA2-256SUMS.sig /tmp/verify-status

FROM rust:1.95-bookworm@sha256:6258907abe69656e41cd992e0b705cdcfabcbbe3db374f92ed2d47121282d4a1 AS builder

WORKDIR /src
RUN apt-get update \
    && apt-get install --no-install-recommends --yes build-essential cmake jq pkg-config \
    && rm -rf /var/lib/apt/lists/*
COPY . .

ARG SOURCE_COMMIT=unknown
ARG IMAGE_VERSION=development
ENV WOTOHA_SOURCE_COMMIT=$SOURCE_COMMIT \
    WOTOHA_BUILD_VERSION=$IMAGE_VERSION

RUN cargo build --locked --release --package wotoha-app --bin wotoha-app \
    && cargo install cargo-about --version 0.9.1 --locked --features cli \
    && cargo metadata --locked --format-version 1 \
        | jq '{schema_version: 1, packages: [.packages[] | {name, version, source, license, license_file, repository}] | sort_by(.name, .version, (.source // ""))}' \
        > /tmp/rust-license-inventory.json \
    && cargo metadata --locked --format-version 1 > /tmp/cargo-metadata.json \
    && cargo about generate --frozen --fail --workspace \
        --config deploy/release-about.toml \
        --output-file /tmp/THIRD_PARTY_LICENSES.html \
        deploy/third-party-licenses.hbs \
    && bash deploy/generate-cargo-attributions.sh \
        /tmp/cargo-metadata.json /tmp/THIRD_PARTY_ATTRIBUTIONS.txt

FROM debian:bookworm-slim@sha256:3783cc01769c7b2b1b83a5c5ad96c815348e28ed7da68e2e3687004faa906251 AS runtime

ARG SOURCE_COMMIT=unknown
ARG IMAGE_VERSION=development

RUN apt-get update \
    && apt-get install --no-install-recommends --yes ca-certificates \
    && rm -rf /var/lib/apt/lists/*

RUN groupadd --system --gid 10001 wotoha \
    && useradd --system --uid 10001 --gid 10001 --home-dir /wotoha --shell /usr/sbin/nologin wotoha \
    && install -d -o 10001 -g 10001 -m 0755 /app /app/tools /app/licenses /app/licenses/rust /app/licenses/yt-dlp /app/licenses/deno /wotoha /wotoha/cache/analysis /wotoha/logs /wotoha/tools /tmp

COPY --from=builder /src/target/release/wotoha-app /app/wotoha-app
COPY --from=tools /out/yt-dlp-fallback /app/tools/yt-dlp-fallback
COPY --from=tools /out/deno /app/tools/deno
COPY LICENSE THIRD_PARTY_NOTICES.md Cargo.lock /app/licenses/
COPY crates/wotoha-runtime/models/LICENSE.beat-this-original.txt \
     crates/wotoha-runtime/models/LICENSE.beat-this-rs.txt \
     crates/wotoha-runtime/models/NOTICE.txt /app/licenses/models/
COPY deploy/third-party-versions.env /app/licenses/
COPY --from=builder /tmp/rust-license-inventory.json /app/licenses/license-inventory.json
COPY --from=builder /tmp/rust-license-inventory.json /app/licenses/rust/license-inventory.json
COPY --from=builder /tmp/THIRD_PARTY_LICENSES.html /app/licenses/rust/THIRD_PARTY_LICENSES.html
COPY --from=builder /tmp/THIRD_PARTY_ATTRIBUTIONS.txt /app/licenses/rust/THIRD_PARTY_ATTRIBUTIONS.txt
COPY --from=tools /out/licenses/yt-dlp/LICENSE /app/licenses/yt-dlp/LICENSE
COPY --from=tools /out/licenses/yt-dlp/PROVENANCE.txt /app/licenses/yt-dlp/PROVENANCE.txt
COPY --from=tools /out/licenses/deno/LICENSE.md /app/licenses/deno/LICENSE.md
COPY --from=tools /out/licenses/deno/PROVENANCE.txt /app/licenses/deno/PROVENANCE.txt

RUN chmod 0555 /app /app/wotoha-app /app/tools /app/tools/yt-dlp-fallback /app/tools/deno /app/licenses \
    && chmod 0555 /app/licenses/models \
    && chmod 0555 /app/licenses/rust /app/licenses/yt-dlp /app/licenses/deno \
    && chmod 0755 /wotoha /wotoha/cache /wotoha/cache/analysis /wotoha/logs /wotoha/tools /tmp \
    && chown -R 10001:10001 /wotoha

ENV WOTOHA_SOURCE_COMMIT=$SOURCE_COMMIT \
    WOTOHA_BUILD_VERSION=$IMAGE_VERSION \
    WOTOHA_IMAGE_VERSION=$IMAGE_VERSION \
    WOTOHA_RECONNECT_STATE_FILE=/wotoha/reconnect.json \
    WOTOHA_ANALYSIS_CACHE_DIR=/wotoha/cache/analysis \
    WOTOHA_LOG_DIR=/wotoha/logs \
    WOTOHA_LOG_FILE_ENABLED=false \
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
VOLUME ["/wotoha"]
ENTRYPOINT ["/app/wotoha-app"]
