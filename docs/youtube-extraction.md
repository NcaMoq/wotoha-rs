# YouTube extraction

Production containers use the immutable image-pinned fallback at
`/app/tools/yt-dlp-fallback` and `/app/tools/deno`. The application image is
immutable and does not self-update. A separately managed optional yt-dlp
override may be placed at `/data/tools/yt-dlp` only when its
`/data/tools/yt-dlp.sha256` sidecar matches the executable. The independent
yt-dlp updater remains a migration-only concern for legacy native hosts.

The yt-dlp resolution order is:

1. `WOTOHA_YTDLP_PATH`: an explicit administrator override, which must be an
   absolute path.
2. The verified managed override `/data/tools/yt-dlp`.
3. The immutable image fallback `/app/tools/yt-dlp-fallback`.
4. The legacy `/opt/wotoha/bin/yt-dlp` path, only when retained for migration.

If the managed executable is present but its sidecar is missing or wrong,
Wotoha emits a diagnostic, refuses that executable, and continues with the
immutable image fallback. The explicit environment override is not silently
replaced: administrators selecting it own its availability and verification.

Wotoha resolves YouTube tracks by starting the official `yt-dlp` executable for each request. `yt-dlp` uses the separately installed Deno runtime when a JavaScript challenge needs to be evaluated. It is always started with `--ignore-config`, so global or user yt-dlp configuration cannot silently change extraction behavior.

The default application settings are a 25-second request deadline and two concurrent yt-dlp processes. `WOTOHA_YTDLP_TIMEOUT_SECONDS` accepts 5–120 seconds and `WOTOHA_YTDLP_CONCURRENCY` accepts 1–8. Cookies are opt-in through `WOTOHA_YTDLP_COOKIES_FILE`; the path must be absolute, name an existing regular file, and have mode `0600` (or otherwise grant no group/other permissions).

The release archives do not redistribute yt-dlp or Deno. The updater-compatible installer downloads the pinned releases directly from their official GitHub repositories during installation, then installs verified copies under `/opt/wotoha/yt-dlp` and exposes the active version through `/opt/wotoha/bin/yt-dlp`. Releases are stored by SHA-256 digest with atomic `current` and `previous` links. yt-dlp metadata and checksums must come from an allowlisted official repository, the checksum signature must match the pinned full release-key fingerprint, and the Deno archive must match its pinned SHA-256 digest. The installer also verifies the reported versions and extraction/direct-media-byte canaries before promotion. `WOTOHA_YTDLP_PATH` is reserved for an explicit administrator override; `WOTOHA_DENO_PATH` selects the Deno executable. An administrator may set absolute paths to different executables, but then owns their verification, updates, and compatibility; the independent updater continues maintaining the standard managed paths without overwriting the override.

`yt-dlp-update.timer` maintains this channel independently of Wotoha releases. A candidate must pass bounded extraction and direct-media-byte canaries before promotion; failure preserves the active version and state. Promotion does not restart the bot, so a healthy running application is not interrupted by an extractor-only update.

The retired native YouTube worker is documented only as migration history in [Ubuntu deployment](ubuntu-deploy.md). Fresh archives contain the application and bootstrap scripts, not yt-dlp or Deno binaries.
