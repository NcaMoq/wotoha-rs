# Wotoha RS — Discord Music Bot with AutoMix, Built in Rust

[日本語](README.ja.md) | English

[![Latest release](https://img.shields.io/github/v/release/NcaMoq/wotoha-rs?sort=semver&label=release)](https://github.com/NcaMoq/wotoha-rs/releases/latest)
[![CI](https://github.com/NcaMoq/wotoha-rs/actions/workflows/ci.yml/badge.svg)](https://github.com/NcaMoq/wotoha-rs/actions/workflows/ci.yml)
[![Rust](https://img.shields.io/badge/Rust-2024-000000?logo=rust)](https://www.rust-lang.org/)

<p align="center">
  <a href="https://discord.com/oauth2/authorize?client_id=1238488423208063107"><img src="https://img.shields.io/badge/Add%20Wotoha%20to%20Discord-5865F2?style=for-the-badge&logo=discord&logoColor=white" alt="Add Wotoha to Discord"></a>
</p>

**Wotoha RS** is a Discord music bot written in Rust. It plays music from YouTube, SoundCloud, Bandcamp, NicoNico, Vimeo, Twitch, and X, then uses audio analysis to create smooth AutoMix transitions and EBU R128/LUFS loudness normalization to keep track volume consistent.

The easiest way to use Wotoha RS is to invite the bot to your Discord server—no Linux server or setup is required.

> If Wotoha RS is useful to you, please [give the repository a star](https://github.com/NcaMoq/wotoha-rs). It helps more Discord and Rust users discover the project.

## Add Wotoha to Your Discord Server

Most users can start with the hosted bot:

1. [Invite Wotoha to Discord](https://discord.com/oauth2/authorize?client_id=1238488423208063107).
2. Select the Discord server where you want to use it and approve the installation.
3. Join a voice channel and run `/play` with a supported music URL.

## Why Wotoha RS?

- **Adaptive AutoMix** — analyzes BPM, beat confidence, musical structure, energy, vocals, and harmonic compatibility before selecting a transition.
- **Safe transition fallback** — chooses beat-matched mixing when it is safe, falls back to an adaptive crossfade, and uses a gapless handoff when an overlap would sound worse.
- **Consistent loudness** — normalizes each track toward `-16 LUFS` by default, with a `-2 dBTP` true-peak ceiling and configurable boost limit.
- **Multi-source playback** — supports YouTube, SoundCloud, Bandcamp, NicoNico, Vimeo, Twitch streams/VODs, and X media URLs.
- **Simple Discord controls** — queue tracks with `/play <url>`, then use Skip, Loop, Shuffle, AutoMix, and List buttons.
- **No server setup** — invite Wotoha to Discord and start playing music without managing a host.
- **Rust audio stack** — built with Tokio, Serenity, Songbird, and Symphonia in a modular Cargo workspace.

## How AutoMix Works

Wotoha RS analyzes the outgoing and incoming tracks before the handoff. The planner evaluates usable intro/outro regions, tempo compatibility, beat and phrase alignment, vocal overlap, energy continuity, and peak headroom.

It then selects the safest available transition:

1. **BeatMatched** — tempo-aware, beat-aligned mixing for compatible tracks.
2. **Crossfade** — an adaptive crossfade when beat matching is not reliable.
3. **Gapless** — a clean handoff when overlapping the tracks would create a poor mix.

A quality guard rejects transitions with unsafe phase drift, vocal collisions, clipping risk, or deep energy dips. Loudness normalization is applied once per track before the final peak guard.

## Supported Music and Media Sources

| Source | Supported content |
| --- | --- |
| YouTube | Videos and playable music URLs through managed yt-dlp |
| SoundCloud | Public tracks |
| Bandcamp | Public track pages |
| NicoNico | Public video URLs |
| Vimeo | Public videos |
| Twitch | Live channels and VODs |
| X / Twitter | Posts containing playable media |

Source availability can change when a provider changes its public interface.

## Discord Usage

Join a voice channel and run:

```text
/play url:https://example.com/music
```

The now-playing message provides these controls:

| Control | Action |
| --- | --- |
| **Skip** | Skip the current track |
| **Loop** | Toggle looping for the current track |
| **Shuffle** | Shuffle queued tracks |
| **AutoMix** | Toggle automatic DJ-style transitions |
| **List** | Show the current track and queue preview |

## Configuration

Wotoha RS reads `.env` during local development and `/etc/wotoha/wotoha.env` in the packaged Linux deployment.

| Variable | Default | Purpose |
| --- | ---: | --- |
| `DISCORD_TOKEN` | required | Discord bot token |
| `WOTOHA_DEFAULT_VOLUME` | `0.10` | Master playback volume |
| `WOTOHA_AUTOMIX_ENABLED` | `true` | Enable AutoMix by default |
| `WOTOHA_AUTOMIX_CROSSFADE_SECONDS` | `8.0` | Preferred maximum crossfade duration |
| `WOTOHA_AUTOMIX_MAX_TEMPO_ADJUSTMENT` | `0.06` | Maximum beat-match tempo adjustment |
| `WOTOHA_AUTOMIX_MIN_BEAT_CONFIDENCE` | `0.70` | Minimum beat confidence for beat matching |
| `WOTOHA_LOUDNESS_NORMALIZATION_ENABLED` | `true` | Enable per-track loudness normalization |
| `WOTOHA_LOUDNESS_TARGET_LUFS` | `-16.0` | Integrated loudness target |
| `WOTOHA_LOUDNESS_MAX_BOOST_DB` | `6.0` | Maximum normalization boost |
| `WOTOHA_LOUDNESS_TRUE_PEAK_CEILING_DBTP` | `-2.0` | True-peak ceiling |
| `WOTOHA_MAX_QUEUE_LEN` | `512` | Maximum queued tracks per Discord server |

See [`deploy/wotoha.env.example`](deploy/wotoha.env.example) for the complete configuration template.

## Build from Source

The repository pins its Rust toolchain in [`rust-toolchain.toml`](rust-toolchain.toml).

```bash
git clone https://github.com/NcaMoq/wotoha-rs.git
cd wotoha-rs
cargo build --release --bin wotoha-app
```

For local development, create a `.env` file with at least `DISCORD_TOKEN`, then run:

```bash
cargo run -p wotoha-app
```

Run the quality gates with:

```bash
cargo fmt --all -- --check
cargo test --workspace --locked
cargo clippy --workspace --all-targets --no-deps -- -D warnings
```

## Linux Deployment Reference

This section is for maintainers operating an independent Wotoha instance. Regular users can simply [add Wotoha to Discord](https://discord.com/oauth2/authorize?client_id=1238488423208063107).

The official release targets **x86_64 Linux**. It provides a portable, application-only archive (`wotoha-linux-x86_64-musl.tar.gz`) and an updater-compatible archive (`wotoha-ubuntu-x86_64-musl.tar.gz`) with systemd units and installation scripts. Neither archive redistributes yt-dlp or Deno. During installation, the updater-compatible installer downloads pinned releases directly from their official GitHub repositories, verifies the yt-dlp signing-key fingerprint and signed checksum plus the pinned Deno SHA-256, runs version and extraction canaries, then installs them atomically. The application binary is statically linked; the installer and updater expect a systemd-based Linux environment with standard GNU utilities. A glibc-based distribution is recommended for the upstream Deno executable.

Install `ca-certificates`, `coreutils`, `curl`, GnuPG, `jq`, `tar`, `unzip`, and `util-linux` with your distribution's package manager. For Debian and Ubuntu:

```bash
sudo apt update
sudo apt install -y ca-certificates coreutils curl gnupg jq tar unzip util-linux
```

To verify an official release before it is extracted, install a current
[GitHub CLI](https://github.com/cli/cli#installation) with
`gh attestation verify` support. Choose a specific published release tag; do
not substitute the moving `latest` download URL for this verification flow.
Replace `vX.Y.Z` below, then complete every command successfully before
extracting or executing the archive:

Use only a release whose Assets list includes the archive, `.sha256`,
`.manifest.json`, and `.intoto.jsonl` files named below. Earlier releases that
lack this complete set do not support this verification procedure.

```bash
(
set -euo pipefail
REPO=NcaMoq/wotoha-rs
TAG=vX.Y.Z
ASSET=wotoha-ubuntu-x86_64-musl.tar.gz
MANIFEST=wotoha-ubuntu-x86_64-musl.manifest.json
BUNDLE=wotoha-ubuntu-x86_64-musl.intoto.jsonl
BASE="https://github.com/$REPO/releases/download/$TAG"

for FILE in "$ASSET" "$ASSET.sha256" "$MANIFEST" "$BUNDLE"; do
  curl --fail --location --remote-name "$BASE/$FILE"
done

gh attestation verify --help | grep -q -- '--deny-self-hosted-runners'
gh attestation verify "$MANIFEST" \
  --bundle "$BUNDLE" --repo "$REPO" \
  --signer-workflow "$REPO/.github/workflows/release.yml" \
  --source-ref "refs/tags/$TAG" --deny-self-hosted-runners
COMMIT="$(jq -er '.commit | select(type == "string" and test("^[0-9a-f]{40}$"))' "$MANIFEST")"
DIGEST="$(sha256sum "$ASSET" | awk '{print $1}')"
SIZE="$(stat --format=%s "$ASSET")"
jq --exit-status --arg tag "$TAG" --arg commit "$COMMIT" \
  --arg asset "$ASSET" --arg digest "$DIGEST" --argjson size "$SIZE" '
  .schema_version == 1 and .tag == $tag and .commit == $commit
  and .asset == $asset and .sha256 == $digest and .size == $size
' "$MANIFEST" >/dev/null
for SUBJECT in "$ASSET" "$MANIFEST"; do
  gh attestation verify "$SUBJECT" \
    --bundle "$BUNDLE" --repo "$REPO" \
    --signer-workflow "$REPO/.github/workflows/release.yml" \
    --source-ref "refs/tags/$TAG" --source-digest "$COMMIT" \
    --deny-self-hosted-runners
done
sha256sum --check --strict "$ASSET.sha256"
)
```

Only after the provenance, manifest, and checksums pass, install the archive:

```bash
tar -xzf wotoha-ubuntu-x86_64-musl.tar.gz
cd wotoha-ubuntu-x86_64-musl
sudo bash ./install-ubuntu.sh
sudoedit /etc/wotoha/wotoha.env
sudo systemctl restart wotoha.service
```

The current operations guide uses Ubuntu commands as a concrete example. For verification, upgrades, rollback behavior, and manual packaging, see the [complete Linux deployment guide](docs/ubuntu-deploy.md).

## Documentation

- [Latest GitHub release](https://github.com/NcaMoq/wotoha-rs/releases/latest)
- [YouTube extraction and managed yt-dlp updates](docs/youtube-extraction.md)
- [Linux deployment and automatic updates (Ubuntu command examples)](docs/ubuntu-deploy.md)
- [Single-host Linux architecture and operations (Ubuntu reference)](docs/single-host-ubuntu.md)

## License

The Wotoha RS project code is available under the [MIT License](LICENSE).
Release archives can include third-party components under their own licenses;
see [Third-Party Notices](THIRD_PARTY_NOTICES.md) for distribution and source
availability information.

## Contributing and Support

Bug reports, playback compatibility reports, feature ideas, and pull requests are welcome through [GitHub Issues](https://github.com/NcaMoq/wotoha-rs/issues). When reporting a media playback problem, include the provider, URL type, Wotoha version, and relevant sanitized logs.

Wotoha RS is a Rust redesign and reimplementation based on the original [Wotoha Discord music bot](https://github.com/NcaMoq/wotoha).
