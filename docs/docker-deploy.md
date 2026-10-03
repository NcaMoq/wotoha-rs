# Docker production deployment

This is the supported production deployment for Wotoha RS: Linux with Docker
on linux/amd64. Native Cargo execution remains supported for Linux development
and CI. NVIDIA, CUDA, cuDNN, TensorRT, NVML, and GPU access are not required
or supported by the production image.

## Pull a digest-pinned image

The canonical image is ghcr.io/ncamoq/wotoha-rs. Prefer a full
content-addressed digest reference (`@sha256:...`) for production. A
`sha-<40-hex-characters>` tag is source-correlated and convenient, but OCI
tags are technically mutable; the digest is the immutable identity.

Create the Compose interpolation file and a private runtime file:

~~~bash
cp .env.example .env
cp runtime.env.example runtime.env
# Set DISCORD_TOKEN in runtime.env and keep that file mode 0600.
chmod 0600 runtime.env
export WOTOHA_IMAGE_REF=ghcr.io/ncamoq/wotoha-rs@sha256:<image-digest>
sudo bash deploy/prepare-docker-data.sh "${WOTOHA_DATA_DIR:-./data}" ./runtime.env
docker run --rm --read-only --tmpfs /tmp:rw,exec,mode=1777 \
  --mount "type=bind,source=${WOTOHA_DATA_DIR:-./data},destination=/wotoha" \
  --cap-drop=ALL --security-opt=no-new-privileges:true \
  "${WOTOHA_IMAGE_REF}" --self-check
docker compose pull
docker compose up -d
~~~

`.env` supplies Compose interpolation such as the digest-pinned image reference and
optional host data directory. `runtime.env` is loaded into the container and
contains the application settings and secret; it is never committed.

## Loudness normalization policy

Loudness normalization remains enabled by default and targets `-16.0` LUFS with
a `-2.0` dBTP ceiling. The default maximum positive gain is now `0.0` dB:
tracks louder than the target are attenuated, while quieter tracks are not
automatically amplified. Set `WOTOHA_LOUDNESS_MAX_BOOST_DB` above `0.0` in
`runtime.env` to opt in to positive gain; the true-peak ceiling applies at
the track-normalization stage. It is not a final-output limiter after the
separately configured master volume, so deployments that raise the master
volume above `1.0` must validate the resulting output chain separately.

This default changed from `6.0` dB to `0.0` dB only when the variable is
omitted. Existing deployments with an explicit value, including `6.0`, retain
their configured behavior.

The Compose file intentionally has no published ports or fake healthcheck.
Discord voice and gateway traffic are outbound connections. The service runs
as UID/GID 10001, drops all Linux capabilities, enables no-new-privileges,
uses a read-only root filesystem, and provides only /tmp as a tmpfs.

## Persistent data and logs

Only /wotoha is writable and persistent:

- /wotoha/cache/analysis contains the bounded analysis cache.
- /wotoha/logs contains the optional secondary runtime log.
- /wotoha/tools is reserved for a separately managed, SHA-256-verified yt-dlp
  override named yt-dlp with a matching yt-dlp.sha256 sidecar.

The production yt-dlp resolver has one explicit, deterministic order:

1. `WOTOHA_YTDLP_PATH`, when an administrator explicitly provides an absolute
   path.
2. `/wotoha/tools/yt-dlp`, only when `/wotoha/tools/yt-dlp.sha256` verifies its
   contents.
3. The immutable image-pinned `/app/tools/yt-dlp-fallback`.
4. The legacy native `/opt/wotoha/bin/yt-dlp` path, only when present for
   migration compatibility.

An invalid managed override is diagnosed and skipped; it is never executed.
The resolver then uses the immutable image fallback when available. The
Compose file passes through `WOTOHA_YTDLP_PATH` only when it is set in the
administrator's environment; it does not force the image fallback through
that variable.

stdout and stderr are the primary logs. Container file logging is disabled by
default; `WOTOHA_LOG_FILE_ENABLED=true` is an explicit optional secondary log
for hosts that need it. The application handles SIGTERM directly as PID 1 and
has a 30-second Compose stop grace period. The image
does not self-update its application binary. A separate yt-dlp updater may
maintain the optional /wotoha/tools override without restarting Wotoha.

## Offline preflight

The image provides a self-check that does not load Discord credentials or make
network requests. It verifies that /wotoha is writable, initializes the embedded
Beat This!/rten models, and runs the pinned yt-dlp and Deno version commands.

~~~bash
docker run --rm --read-only --tmpfs /tmp:rw,exec,mode=1777 \
  --mount type=tmpfs,destination=/wotoha,tmpfs-mode=0777 \
  --cap-drop=ALL --security-opt=no-new-privileges:true \
  ghcr.io/ncamoq/wotoha-rs@sha256:<image-digest> --self-check
~~~

The container workflow performs the same check on a linux/amd64 image and
also verifies that Rust/Cargo/source/debug artifacts are absent from the final
stage.

## Upgrade and rollback

Record the source commit, source-correlated tag, and resolved image digest.
Set WOTOHA_IMAGE_REF to the new digest reference, then pull and recreate:

~~~bash
export WOTOHA_IMAGE_REF=ghcr.io/ncamoq/wotoha-rs@sha256:<new-image-digest>
docker compose pull
docker compose up -d
docker compose logs --since=5m wotoha
~~~

To roll back, set the old digest reference and repeat the same commands. The
host `./data` directory is retained across image changes.

### Migrating the former named volume

Older Compose deployments used a Docker-managed volume for the persistent
data. Before the first container recreation, copy that volume into the host
directory that will be mounted at `/wotoha`; do not mount both stores at the
same time:

~~~bash
mkdir -p /home/ncamoq/wotoha/data
docker run --rm \
  --mount source=wotoha-data,destination=/from,readonly \
  --mount type=bind,source=/home/ncamoq/wotoha/data,destination=/to \
  debian:bookworm-slim \
  sh -c 'cp -a /from/. /to/'
~~~

If the old volume has a different name, replace only `source=` after checking
`docker volume ls`. Inspect the copy, set `WOTOHA_DATA_DIR=/home/ncamoq/wotoha/data`
in `.env`, and start the new Compose service. Keep the old volume untouched
until the new container has passed `--self-check` and has operated normally
through one restart; after that, remove it only under the host's normal change
procedure. The application writes to one `/wotoha` mount and does not perform
dual writes during migration.

## Building locally

The Dockerfile is a multi-stage build. The builder compiles the locked
wotoha-app binary; the tools stage downloads the pinned yt-dlp and Deno
artifacts, verifies the yt-dlp signed checksum and signing-key fingerprint,
verifies the pinned Deno SHA-256, and checks reported versions. The final
Debian slim stage contains only the application, licenses, runtime tools, and
data directories.

~~~bash
docker buildx build --platform linux/amd64 --load \
  --build-arg SOURCE_COMMIT=$(git rev-parse HEAD) \
  --build-arg IMAGE_VERSION=local \
  --tag wotoha-rs:local .
docker run --rm --read-only --tmpfs /tmp:rw,exec,mode=1777 \
  --mount type=tmpfs,destination=/wotoha,tmpfs-mode=0777 \
  wotoha-rs:local --self-check
~~~

The GitHub workflow builds and smoke-tests pull-request images without
pushing. Pushes to main publish a source-correlated SHA tag and its content
digest. Version tags publish both the version tag and the source-correlated SHA
tag; production should still use the recorded digest reference.
