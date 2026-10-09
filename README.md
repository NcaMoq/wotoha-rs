# Wotoha RS — Automatic DJ for Discord, built in Rust

[日本語](README.ja.md) · English

**Wotoha RS** is an open-source **Automatic DJ for Discord**. It is built in Rust and uses audio analysis to plan track handoffs instead of applying one fixed crossfade to every song: BPM/tempo and beat evidence, structure, energy, vocal activity, tonal information, and loudness safety all contribute where available.

<p align="center">
  <a href="https://discord.com/oauth2/authorize?client_id=1238488423208063107"><img src="https://img.shields.io/badge/Add%20Wotoha%20to%20Discord-5865F2?style=for-the-badge&logo=discord&logoColor=white" alt="Add Wotoha to Discord"></a>
</p>

[![Latest release](https://img.shields.io/github/v/release/NcaMoq/wotoha-rs?sort=semver&label=release)](https://github.com/NcaMoq/wotoha-rs/releases/latest)
[![CI](https://github.com/NcaMoq/wotoha-rs/actions/workflows/ci.yml/badge.svg)](https://github.com/NcaMoq/wotoha-rs/actions/workflows/ci.yml)
[![Rust](https://img.shields.io/badge/Rust-2024-000000?logo=rust)](https://www.rust-lang.org/)
[![License](https://img.shields.io/github/license/NcaMoq/wotoha-rs)](LICENSE)

The result is a bot that can choose a beat-aware **BeatMatched** transition, an adaptive **Crossfade**, or a **Gapless** fallback when overlapping the tracks is not a good choice. It plays media from common public sources and can be used immediately by inviting it to a server or self-hosted with Docker.

> If Wotoha RS is useful to you, consider [starring the repository](https://github.com/NcaMoq/wotoha-rs).

The [latest release](https://github.com/NcaMoq/wotoha-rs/releases/latest) is the stable distribution path. `main` can contain unreleased engineering work; use the release notes when preparing a production deployment.

## Why Wotoha RS?

- **Analysis-driven AutoMix** — transition candidates use tempo, beat timing, structure, cues, energy, vocal activity, and optional tonal compatibility rather than a single global fade setting.
- **Conservative handoffs** — BeatMatched is used only when the timing, overlap, and quality checks support it; otherwise the planner falls back to Crossfade or Gapless.
- **Loudness-aware playback** — track normalization targets `-16 LUFS` by default, with an attenuation-first policy and a `-2 dBTP` true-peak ceiling at the normalization stage.
- **Rust audio stack** — a modular Cargo workspace built around Tokio, Serenity, Songbird, and Symphonia.
- **Self-hostable** — the production container runs on Linux/amd64 with a non-root user, a read-only root filesystem, dropped capabilities, and an offline self-check.

## AutoMix at a glance

```text
Outgoing track             Incoming track
      │                           │
      └────── decode + analysis ──┘
             beat / tempo / phase
          structure / cue / energy
          vocal / tonal / loudness
                         │
                 bounded planner
                 ┌────────┼────────┐
                 │        │        │
            BeatMatched Crossfade Gapless
                 └────────┼────────┘
                   quality + peak guards
                         │
                       playback
```

The default production authority is the compatibility planner. AutoMix V2 can be observed in `shadow` mode or selected explicitly with `WOTOHA_AUTOMIX_PLANNER_MODE=v2` for controlled evaluation; V2 is not silently enabled by the research lab. The implementation boundary and these modes are documented in [docs/automix.md](docs/automix.md).

## Supported sources

| Source | Supported content |
| --- | --- |
| YouTube | Videos and playable music URLs through managed yt-dlp |
| SoundCloud | Public tracks |
| Bandcamp | Public track pages |
| NicoNico | Public video URLs |
| Vimeo | Public videos |
| Twitch | Live channels and VODs |
| X / Twitter | Posts containing playable media |

Provider availability follows each service's public interfaces and can change independently of Wotoha.

## Use it in Discord

1. [Invite Wotoha to Discord](https://discord.com/oauth2/authorize?client_id=1238488423208063107).
2. Choose a server and approve the installation.
3. Join a voice channel and run:

```text
/play url:https://example.com/music
```

Playback controls are exposed from the now-playing message, including Skip, Loop, Shuffle, AutoMix, and queue actions.

## Self-hosting

Production deployment uses Docker on Linux/amd64. Start with [the Docker deployment guide](docs/docker-deploy.md), [`compose.yaml`](compose.yaml), and [`runtime.env.example`](runtime.env.example). The long-form upgrade, rollback, data, and security procedure stays in the deployment documentation.

For local development and a source build, see [docs/development.md](docs/development.md). Legacy host migration details are kept in [docs/ubuntu-deploy.md](docs/ubuntu-deploy.md).

## Audio analysis research

The repository includes a clean-room analysis lab for testing beat and tempo changes against controlled evidence before they are considered for production. Research commands, generated artifacts, and external reference packets are separate from the playback authority; see [docs/analysis-lab.md](docs/analysis-lab.md).

## Demo

There is no recorded demo asset in the repository yet. [docs/demo-capture.md](docs/demo-capture.md) describes a small, honest capture that would show the analysis inputs and selected handoff without inventing benchmark claims.

## Documentation and contributing

- [AutoMix architecture](docs/automix.md)
- [Development environment](docs/development.md)
- [Docker deployment](docs/docker-deploy.md)
- [Provider and YouTube extraction notes](docs/youtube-extraction.md)
- [Security policy](SECURITY.md) · [Privacy policy](PRIVACY.md)
- [Contributing](CONTRIBUTING.md)

Issues and pull requests are welcome. Keep credentials, private media, raw external observations, generated reports, and build output outside Git.

## License

Wotoha RS is distributed under the [MIT License](LICENSE). Third-party notices are collected in [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md).
