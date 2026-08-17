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
