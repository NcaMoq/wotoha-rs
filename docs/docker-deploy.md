# Docker production deployment

This is the supported production deployment for Wotoha RS: Linux with Docker
on linux/amd64. Native Cargo execution remains supported for Linux development
and CI. NVIDIA, CUDA, cuDNN, TensorRT, NVML, and GPU access are not required
or supported by the production image.

## Pull an immutable image

The canonical image is ghcr.io/ncamoq/wotoha-rs. Use a full SHA tag
sha-<40-hex-characters> or a release version tag. Do not use latest for an
operational deployment.

Create the Compose interpolation file and a private runtime file:

~~~bash
cp .env.example .env
cp runtime.env.example runtime.env
# Set DISCORD_TOKEN in runtime.env and keep that file mode 0600.
chmod 0600 runtime.env
export WOTOHA_IMAGE_TAG=sha-<full-git-sha>
docker compose pull
docker compose up -d
~~~

`.env` supplies Compose interpolation such as the immutable image tag and
optional host data directory. `runtime.env` is loaded into the container and
contains the application settings and secret; it is never committed.

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

stdout and stderr are the primary logs. The application handles SIGTERM
directly as PID 1 and has a 30-second Compose stop grace period. The image
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
pushing. Pushes to main publish the immutable SHA tag. Version tags publish
both the version tag and the immutable SHA tag to the canonical GHCR image.
