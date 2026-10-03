# Legacy native Linux migration

This document is retained for existing native Linux installations only.
Supported production deployment is the Linux + Docker image documented in
docker-deploy.md. New hosts must not install the native systemd service or the
native Wotoha application updater.

## Migration

1. Record the current immutable Wotoha image tag and Discord configuration.
2. Install Docker Engine and Compose on the Linux host.
3. Copy compose.yaml, `.env.example`, and `runtime.env.example` to the host.
4. Set `DISCORD_TOKEN` in `runtime.env` and an immutable `WOTOHA_IMAGE_TAG` in `.env`.
5. Run the offline self-check before connecting the bot.
6. Start the service with docker compose up -d.
7. After observing healthy logs, stop and disable the old native service.

The host `./data` directory, mounted at `/wotoha`, is the persistent boundary
for the container. Move any analysis cache, reconnect state, or operational
logs that must be retained into that directory; the application binary and its
container filesystem are immutable.

## Legacy updater boundary

The native application updater is disabled for supported deployments. It is
kept in the repository only as a guarded migration aid for hosts that still
need to remove or inspect an older installation. It must not replace the
application binary in a production rollout.

The independent yt-dlp updater is separate from application release
management. It may continue to maintain a verified extractor on a legacy
host, and its signed checksum, pinned Deno digest, version checks, and
extraction canaries must remain enabled until the host has migrated to the
container image. The container ships its own verified pinned fallback and
does not self-update the application.

## Removal after migration

After the container has started successfully and the old service is stopped,
remove only the old native Wotoha service and application files according to
the host's change procedure. Preserve any records required by the operator.
Do not remove the Docker data volume during a rollback or an image upgrade.

For image upgrade and rollback procedures, see docker-deploy.md. For the
container's yt-dlp layout and optional verified override, see
youtube-extraction.md.
