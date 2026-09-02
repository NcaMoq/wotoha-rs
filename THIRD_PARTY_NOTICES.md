# Third-Party Notices

Wotoha is distributed under the MIT License; see `LICENSE`.

This file describes third-party material intentionally shipped in Wotoha
release archives. It is an attribution and distribution record, not legal
advice or a substitute for a downstream distributor's license review.

## Release artifacts

`wotoha-linux-x86_64-musl.tar.gz` contains the portable Wotoha application,
the Wotoha MIT License, this notice, and locked Rust dependency inventory and
license-text material.

`wotoha-ubuntu-x86_64-musl.tar.gz` retains the filename, top-level directory,
deployment scripts, and updater contract used by existing installations.

Neither archive contains yt-dlp, Deno, their release checksum payloads, or
their binary archives. Wotoha's release process therefore does not
redistribute those third-party executables.

## Runtime bootstrap of yt-dlp and Deno

The updater-compatible archive retains `install-yt-dlp-bundle.sh` as a
bootstrap entry point. At installation time it reads the packaged, pinned
versions and digests from `deploy/third-party-versions.env`, then downloads
yt-dlp and Deno directly from their official GitHub release repositories.

Before installation, the bootstrap verifies the yt-dlp release checksum's GPG
signature with the packaged upstream public key, verifies the exact yt-dlp
binary digest from that signed checksum, verifies the pinned Deno archive
digest, checks both reported versions, and runs the extraction/direct-byte
canary. The independently downloaded programs remain subject to their upstream
licenses and are not part of the Wotoha release archive.

## Rust application dependencies

Both Linux archives include:

- `third-party/rust/Cargo.lock`;
- `third-party/rust/license-inventory.json`, generated from locked Cargo
  metadata; and
- `third-party/rust/THIRD_PARTY_LICENSES.html`, generated with cargo-about
  0.9.1 from the locked dependency graph. It provides dependency attribution,
  full license texts, `used_by` information, and versioned crates.io links; and
- `third-party/rust/THIRD_PARTY_ATTRIBUTIONS.txt`, generated from standalone
  `COPYRIGHT` and `NOTICE` files shipped by the locked Rust dependencies.

The release gate rejects a package whose Cargo metadata declares neither a
license expression nor a license file. The JSON inventory is machine-readable
license traceability; the accompanying HTML and attribution text are the
human-readable license and notice bundle. They are not a standards-certified
SBOM, a source bundle, or a license-compatibility determination. Downstream
binary redistributors must review the material and satisfy the applicable
notice and source-availability terms for their distribution.

### Beat This! embedded model assets

The neural beat detector embeds two ONNX assets from the pinned
[beat-this-rs commit `089b509247e6fdcec666511c0dcf0d5f39c21e73`](https://github.com/danigb/beat-this-rs/tree/089b509247e6fdcec666511c0dcf0d5f39c21e73)
using `include_bytes!`; release archives therefore contain the model in the
executable and do not duplicate the approximately 10 MiB `.onnx` file. The
copyright attribution and complete pinned MIT license texts are included in
the archive's `third-party/neural-models/` bundle. No training files or datasets are included.
CPJKU's primary README says that the code and
published model weights are MIT-licensed while noting that some training files
may have separate terms; downstream distributors should review the terms that
apply to their distribution.

| Embedded asset | SHA-256 | Size | Upstream Git blob |
| --- | --- | ---: | --- |
| `beat_this_small.onnx` | `a5f8d39d989f31859454ba27afe61c5317ca95e4d9373e6853e5361b8937172f` | 10,555,592 | `4f43223f38751cdb40ed1d7cff44acccaf7e3794` |
| `mel_spectrogram.onnx` | `fdd59e65c515331308e4c8841edf99972deca646bdf6197744c2a5b7755e3de9` | 270,742 | `d54915ce662785df07343af176cb61be61283448` |

The files originate at the pinned commit's
[`models/` paths](https://github.com/danigb/beat-this-rs/tree/089b509247e6fdcec666511c0dcf0d5f39c21e73/models).
The pinned conversion script documents that they are exports of official
Beat This! checkpoints from the JKU cloud:
[`scripts/ckpt2onnx.py`](https://github.com/danigb/beat-this-rs/blob/089b509247e6fdcec666511c0dcf0d5f39c21e73/scripts/ckpt2onnx.py).
The original project's primary
[license section at the model-source commit `b95c8ab0c58c2d9fcfd40508ae8dffbc05ac4f5c`](https://github.com/CPJKU/beat_this/blob/b95c8ab0c58c2d9fcfd40508ae8dffbc05ac4f5c/README.md#license)
states that its code and published model weights are MIT-licensed and carries
`Copyright (c) 2024 Institute of Computational Perception, JKU Linz, Austria`.
The original MIT text is retained from the corresponding
[LICENSE commit `72f586c02402bce53cb9bf30029bd4c2f620efa0`](https://github.com/CPJKU/beat_this/blob/72f586c02402bce53cb9bf30029bd4c2f620efa0/LICENSE).
The corresponding reference paper is
["Beat This! Accurate Beat Tracking Without DBN Postprocessing"](https://arxiv.org/abs/2407.21658)
(ISMIR 2024).
The pinned port's
[LICENSE](https://github.com/danigb/beat-this-rs/blob/089b509247e6fdcec666511c0dcf0d5f39c21e73/LICENSE)
retains that notice and adds `Copyright (c) 2025 danigb (Rust port)`. The
archive's model bundle includes complete copies of both texts, not links only.

The embedded runtime uses RTen `0.24.0`. RTen and its `rten-tensor`
subcrate declare `MIT OR Apache-2.0` in the pinned upstream
[workspace manifest](https://github.com/robertknight/rten/blob/v0.24.0/Cargo.toml)
and [tensor manifest](https://github.com/robertknight/rten/blob/v0.24.0/rten-tensor/Cargo.toml).
All RTen `0.24.0` crates in the locked graph are covered by the generated
Rust license bundle above; no additional model-specific license is inferred
from RTen's runtime license.

### MPL-2.0 source availability

The locked release dependency graph includes the Mozilla Public License 2.0
(MPL-2.0) components below. When a distributed Wotoha executable includes
Covered Software from one of these crates, corresponding source for the exact
listed version is available from its versioned crates.io download link. These
links provide the source-location notice for Executable Form distribution under
MPL-2.0 section 3.2. Wotoha does not modify the listed third-party crates.

| Component | Version | Corresponding Source |
| --- | ---: | --- |
| [hpke-rs](https://crates.io/crates/hpke-rs/0.6.1) | `0.6.1` | [download](https://crates.io/api/v1/crates/hpke-rs/0.6.1/download) |
| [hpke-rs-crypto](https://crates.io/crates/hpke-rs-crypto/0.6.1) | `0.6.1` | [download](https://crates.io/api/v1/crates/hpke-rs-crypto/0.6.1/download) |
| [hpke-rs-libcrux](https://crates.io/crates/hpke-rs-libcrux/0.6.1) | `0.6.1` | [download](https://crates.io/api/v1/crates/hpke-rs-libcrux/0.6.1/download) |
| [hpke-rs-rust-crypto](https://crates.io/crates/hpke-rs-rust-crypto/0.6.1) | `0.6.1` | [download](https://crates.io/api/v1/crates/hpke-rs-rust-crypto/0.6.1/download) |
| [symphonia](https://crates.io/crates/symphonia/0.5.5) | `0.5.5` | [download](https://crates.io/api/v1/crates/symphonia/0.5.5/download) |
| [symphonia-bundle-flac](https://crates.io/crates/symphonia-bundle-flac/0.5.5) | `0.5.5` | [download](https://crates.io/api/v1/crates/symphonia-bundle-flac/0.5.5/download) |
| [symphonia-bundle-mp3](https://crates.io/crates/symphonia-bundle-mp3/0.5.5) | `0.5.5` | [download](https://crates.io/api/v1/crates/symphonia-bundle-mp3/0.5.5/download) |
| [symphonia-codec-aac](https://crates.io/crates/symphonia-codec-aac/0.5.5) | `0.5.5` | [download](https://crates.io/api/v1/crates/symphonia-codec-aac/0.5.5/download) |
| [symphonia-codec-adpcm](https://crates.io/crates/symphonia-codec-adpcm/0.5.5) | `0.5.5` | [download](https://crates.io/api/v1/crates/symphonia-codec-adpcm/0.5.5/download) |
| [symphonia-codec-alac](https://crates.io/crates/symphonia-codec-alac/0.5.5) | `0.5.5` | [download](https://crates.io/api/v1/crates/symphonia-codec-alac/0.5.5/download) |
| [symphonia-codec-pcm](https://crates.io/crates/symphonia-codec-pcm/0.5.5) | `0.5.5` | [download](https://crates.io/api/v1/crates/symphonia-codec-pcm/0.5.5/download) |
| [symphonia-codec-vorbis](https://crates.io/crates/symphonia-codec-vorbis/0.5.5) | `0.5.5` | [download](https://crates.io/api/v1/crates/symphonia-codec-vorbis/0.5.5/download) |
| [symphonia-core](https://crates.io/crates/symphonia-core/0.5.5) | `0.5.5` | [download](https://crates.io/api/v1/crates/symphonia-core/0.5.5/download) |
| [symphonia-format-isomp4](https://crates.io/crates/symphonia-format-isomp4/0.5.5) | `0.5.5` | [download](https://crates.io/api/v1/crates/symphonia-format-isomp4/0.5.5/download) |
| [symphonia-format-mkv](https://crates.io/crates/symphonia-format-mkv/0.5.5) | `0.5.5` | [download](https://crates.io/api/v1/crates/symphonia-format-mkv/0.5.5/download) |
| [symphonia-format-ogg](https://crates.io/crates/symphonia-format-ogg/0.5.5) | `0.5.5` | [download](https://crates.io/api/v1/crates/symphonia-format-ogg/0.5.5/download) |
| [symphonia-format-riff](https://crates.io/crates/symphonia-format-riff/0.5.5) | `0.5.5` | [download](https://crates.io/api/v1/crates/symphonia-format-riff/0.5.5/download) |
| [symphonia-metadata](https://crates.io/crates/symphonia-metadata/0.5.5) | `0.5.5` | [download](https://crates.io/api/v1/crates/symphonia-metadata/0.5.5/download) |
| [symphonia-utils-xiph](https://crates.io/crates/symphonia-utils-xiph/0.5.5) | `0.5.5` | [download](https://crates.io/api/v1/crates/symphonia-utils-xiph/0.5.5/download) |

Upstream project repositories are [hpke-rs](https://github.com/cryspen/hpke-rs)
and [Symphonia](https://github.com/pdeljanov/Symphonia). The
[MPL-2.0 terms](https://www.mozilla.org/MPL/2.0/) govern these components;
Mozilla's [MPL FAQ](https://www.mozilla.org/en-US/MPL/2.0/FAQ/) provides
additional context about distributing Executable Form.
