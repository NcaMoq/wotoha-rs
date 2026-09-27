# Docker production deployment

This is the supported production deployment for Wotoha RS: Linux with Docker
on linux/amd64. Native Cargo execution remains supported for Linux development
and CI. NVIDIA, CUDA, cuDNN, TensorRT, NVML, and GPU access are not required
or supported by the production image.

## Pull an immutable image

The canonical image is ghcr.io/ncamoq/wotoha-rs. Use a full SHA tag
sha-<40-hex-characters> or a release version tag. Do not use latest for an
operational deployment.

Create a local environment file and set the Discord token:

~~~bash
cp .env.example .env
# Set DISCORD_TOKEN in .env before continuing.
export WOTOHA_IMAGE_TAG=sha-<full-git-sha>
docker compose pull
docker compose up -d
~~~

The Compose file intentionally has no published ports or fake healthcheck.
Discord voice and gateway traffic are outbound connections. The service runs
as UID/GID 10001, drops all Linux capabilities, enables no-new-privileges,
uses a read-only root filesystem, and provides only /tmp as a tmpfs.

## Persistent data and logs

Only /data is writable and persistent:

- /data/cache/analysis contains the bounded analysis cache.
- /data/logs contains the optional secondary runtime log.
- /data/tools is reserved for a separately managed, SHA-256-verified yt-dlp
  override named yt-dlp with a matching yt-dlp.sha256 sidecar.

stdout and stderr are the primary logs. The application handles SIGTERM
directly as PID 1 and has a 30-second Compose stop grace period. The image
does not self-update its application binary. A separate yt-dlp updater may
maintain the optional /data/tools override without restarting Wotoha.

## Offline preflight

The image provides a self-check that does not load Discord credentials or make
network requests. It verifies that /data is writable, initializes the embedded
Beat This!/rten models, and runs the pinned yt-dlp and Deno version commands.

~~~bash
docker run --rm --read-only --tmpfs /tmp:rw,exec,mode=1777 \
  --mount type=tmpfs,destination=/data,tmpfs-mode=0777 \
  --cap-drop=ALL --security-opt=no-new-privileges:true \
  ghcr.io/ncamoq/wotoha-rs:sha-<full-git-sha> --self-check
~~~

The container workflow performs the same check on a linux/amd64 image and
also verifies that Rust/Cargo/source/debug artifacts are absent from the final
stage.

## Upgrade and rollback

Set WOTOHA_IMAGE_TAG to the new immutable tag, then pull and recreate:

~~~bash
export WOTOHA_IMAGE_TAG=sha-<new-full-git-sha>
docker compose pull
docker compose up -d
docker compose logs --since=5m wotoha
~~~

To roll back, set the old immutable tag and repeat the same commands. The
named wotoha-data volume is retained across image changes.

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
  --mount type=tmpfs,destination=/data,tmpfs-mode=0777 \
  wotoha-rs:local --self-check
~~~

The GitHub workflow builds and smoke-tests pull-request images without
pushing. Pushes to main publish the immutable SHA tag. Version tags publish
both the version tag and the immutable SHA tag to the canonical GHCR image.
