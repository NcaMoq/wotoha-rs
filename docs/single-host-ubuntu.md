# Single-Host Ubuntu Notes

This repository is being shaped for a single Ubuntu server deployment with the same Discord UX as the original Java bot.

## What is already tightened

- The runtime is now split into explicit `app`, `contracts`, `control`, `media`, `voice`, and `core` crates.
- `control` no longer depends on the concrete `voice` implementation. It talks to a `PlaybackService` contract.
- `voice` no longer depends on the concrete `media` implementation. It talks to a `MediaBackend` contract.
- `TrackRequest` and `TrackMetadata` now use shared string storage so queue mutations, loop transitions, and queue previews avoid deep string copies.
- Media resolution is split into a metadata probe path and a playback input path. This removes the eager creation of playback inputs for queued tracks.
- Media providers now sit behind a registry and resolver facade. The first provider is still `yt-dlp`, but the replacement boundary is explicit.
- Resolver metadata is cached in memory with in-flight request deduplication and bounded probe concurrency.
- Playback sessions separate playback state from voice occupancy state, use dedicated locks for each, avoid auto-creating sessions on read-only paths, and reject stale callbacks with per-session ids.
- Enqueue now reserves order first and resolves media outside the guild mutation lock, then commits tracks in ticket order.
- Voice occupancy is bootstrapped once per active guild and then updated incrementally in O(1) time from voice state events.
- Queue rendering now uses a bounded preview instead of cloning the whole guild queue.
- Startup command sync is idempotent and no longer bulk-overwrites global commands on every reconnect.
- Serenity cache settings are tightened to keep guild state while disabling channel and user caches.
- Release builds now use `lto=fat`, `codegen-units=1`, `panic=abort`, and symbol stripping.

## What is still a real compromise

- The media backend still relies on `songbird::input::YoutubeDl`, which means `yt-dlp` is still in the hot path. That is not a no-compromise final state.
- Gateway and interactions are still handled inside a Serenity client. A stricter single-host design would split webhook interactions, gateway control, and voice work.
- Voice transport still depends on Songbird's internal scheduler and driver instead of a fully custom Rust voice runtime.

## Before claiming "no compromise"

The following work still needs to happen:

1. Replace the `yt-dlp` path with Rust-native providers and prepared-source caching.
2. Move interactions to HTTP webhook ingress and shrink gateway responsibilities to guild and voice state traffic.
3. Split the single process into single-host worker roles once the compatibility surface is stable.
4. Replace Songbird's runtime-facing transport path with a dedicated Rust voice worker when compatibility is locked down.

## Ubuntu deployment baseline

- Build with `cargo build --release --bin wotoha-app`
- Install the binary at `/opt/wotoha/bin/wotoha-app`
- Install `deploy/wotoha.service` as `/etc/systemd/system/wotoha.service`
- Put `DISCORD_TOKEN=...` in `/etc/wotoha/wotoha.env`
- Run `sudo systemctl daemon-reload && sudo systemctl enable --now wotoha.service`
