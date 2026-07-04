# Single-Host Ubuntu Notes

This repository is being shaped for a single Ubuntu server deployment with the same Discord UX as the original Java bot.

## What is already tightened

- The runtime is now split into explicit `app`, `contracts`, `control`, `media`, `voice`, and `core` crates.
- `control` no longer depends on the concrete `voice` implementation. It talks to a `PlaybackService` contract.
- `voice` no longer depends on the concrete `media` implementation. It talks to a `MediaBackend` contract.
- `TrackRequest` and `TrackMetadata` now use shared string storage so queue mutations, loop transitions, and queue previews avoid deep string copies.
- `TrackRequest` now carries provider-scoped canonical identity and URL fields, so metadata and playback can stop treating raw slash-command URLs as durable cache keys.
- Media resolution is split into a metadata probe path and a playback input path. This removes the eager creation of playback inputs for queued tracks.
- Media providers now sit behind a registry and resolver facade with native extraction only. There is no `yt-dlp` fallback left in the workspace.
- Resolver metadata is cached in memory with canonical-key storage, raw-url aliasing, in-flight request deduplication, and bounded probe concurrency.
- Playback input creation is now async and expiry-aware, so signed provider URLs can be refreshed at play time instead of being frozen when a track is first queued.
- Native providers now exist for YouTube, SoundCloud, Bandcamp, NicoNico, X/Twitter, Vimeo, and Twitch.
- Bandcamp track pages now resolve natively to prepared HTTP playback instead of going straight through `yt-dlp`.
- Playback sessions separate playback state from voice occupancy state, use dedicated locks for each, avoid auto-creating sessions on read-only paths, and reject stale callbacks with per-session ids.
- Enqueue now reserves order first and resolves media outside the guild mutation lock, then commits tracks in ticket order.
- Voice occupancy is bootstrapped once per active guild and then updated incrementally in O(1) time from voice state events.
- Queue rendering now uses a bounded preview instead of cloning the whole guild queue, and it truncates content before Discord embed limits are hit.
- Startup command sync is idempotent and no longer bulk-overwrites global commands on every reconnect.
- Serenity cache settings are tightened to keep guild state while disabling channel and user caches.
- Slash commands and button actions now enforce same-room control rules instead of letting another voice channel steal or manipulate the active session.
- Prepared playback now has its own short-lived cache, canonical-key dedupe, and bounded refresh path instead of forcing a provider re-fetch on every handoff to runtime.
- Runtime diagnostics now flow through tracing only, and signed media URLs are redacted before they are written to disk.
- Release builds now use `lto=fat`, `codegen-units=1`, `panic=abort`, and symbol stripping.

## What is still a real compromise

- Gateway and interactions are still handled inside a Serenity client. A stricter single-host design would split webhook interactions, gateway control, and voice work.
- Voice transport still depends on Songbird's internal scheduler and driver instead of a fully custom Rust voice runtime.
- The X/Twitter provider still depends on undocumented guest-access endpoints and dynamic page assets. It is Rust-native, but it is the least stable provider in the set and needs continued verification.

## Before claiming "no compromise"

The following work still needs to happen:

1. Move interactions to HTTP webhook ingress and shrink gateway responsibilities to guild and voice state traffic.
2. Split the single process into single-host worker roles once the compatibility surface is stable.
3. Replace Songbird's runtime-facing transport path with a dedicated Rust voice worker when compatibility is locked down.

## Ubuntu deployment baseline

- Build with `cargo build --release --bin wotoha-app`
- Install the binary at `/opt/wotoha/bin/wotoha-app`
- Install `deploy/wotoha.service` as `/etc/systemd/system/wotoha.service`
- Put `DISCORD_TOKEN=...` and the operational settings from `deploy/wotoha.env.example` in `/etc/wotoha/wotoha.env`
- Let `systemd` own `/var/lib/wotoha` for state and `/var/log/wotoha` for logs instead of writing under the install directory.
- Run `sudo systemctl daemon-reload && sudo systemctl enable --now wotoha.service`

## Runtime environment settings

`deploy/wotoha.env.example` contains the settings read at startup:

- `DISCORD_TOKEN`: Discord bot token.
- `RUST_LOG`: tracing filter used by stdout and `/var/log/wotoha/wotoha-app.runtime.log`.
- `WOTOHA_LOG_DIR`: directory for the runtime log file.
- `WOTOHA_LOG_FILE`: file name created under `WOTOHA_LOG_DIR`. Directory separators are rejected.
- `WOTOHA_LOG_ANSI`: `true` enables ANSI color sequences in stdout and the file writer; keep `false` for systemd logs.
- `WOTOHA_DEFAULT_VOLUME`: playback volume value, accepted range `0.0..=2.0`.
- `WOTOHA_MAX_QUEUE_LEN`: guild queue limit value, accepted range `1..=512`.
- `WOTOHA_MAX_PENDING_ENQUEUES`: pending enqueue limit value, accepted range `1..=64`. It cannot exceed `WOTOHA_MAX_QUEUE_LEN`.

Startup applies these values before the Discord client is built. Logging settings configure both stdout and the runtime log file. Playback volume is applied through the runtime track handle. Queue and pending enqueue limits are checked before enqueue work enters the playback coordinator.
