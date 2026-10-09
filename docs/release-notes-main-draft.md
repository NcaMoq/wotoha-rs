# Unreleased main release notes — draft only

The current candidate release draft is [v0.6.0 — AutoMix V2](release-notes-v0.6.0-draft.md).

> This document is a maintainer draft for the current `main` branch. It is
> not a published release and does not replace the latest tagged release.

The latest published release is [v0.5.36](https://github.com/NcaMoq/wotoha-rs/releases/tag/v0.5.36).
The current branch is substantially ahead of that tag, so a future release
should be cut and tested from a deliberate release commit rather than treating
`main` as a stable version.

## What changed since v0.5.36

- Docker is the supported production deployment unit for Linux/amd64, with a
  digest-pinned image workflow, non-root execution, read-only filesystem,
  dropped capabilities, and offline self-check coverage.
- Runtime delivery, reconnect state, bounded persistence, and managed media
  helper handling have additional safety and provenance checks.
- Loudness normalization defaults to attenuation-only behavior; positive gain
  remains an explicit configuration choice and true-peak limits remain part of
  the safety boundary.
- AutoMix planning has a documented V2 timeline/cue analysis surface and
  research shadow controls. The compatibility planner remains the default
  production authority until a separate promotion decision is made.
- The analysis lab now provides deterministic known-truth evaluation,
  leakage-aware research reports, and fixed-tempo diagnostics. These reports
  are research evidence, not production configuration.
- Clean-room external references are represented through neutral observation
  packets; vendor-specific acquisition material remains outside the repository.

## Upgrade notes

- New production deployments should follow [Docker production deployment](docker-deploy.md)
  and use an immutable image digest.
- Existing native Linux installations are supported only through the documented
  migration path. Do not treat the legacy archive workflow as the preferred
  deployment model for a new host.
- If `WOTOHA_LOUDNESS_MAX_BOOST_DB` is omitted, the current default is `0.0`.
  Deployments that explicitly set another value retain that configured value.
- Review the release asset manifest, checksum, provenance, and container smoke
  results before publishing a future tag.

## Not claimed by this draft

This draft does not claim that the research planner is production-ready, that
synthetic analysis generalizes to all real music, or that the current `main`
branch is a substitute for a tagged release. Those claims require separate
validation and release approval.
