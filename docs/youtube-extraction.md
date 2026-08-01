# YouTube extraction

Wotoha resolves YouTube tracks by starting the official `yt-dlp` executable for each request. `yt-dlp` uses the separately installed Deno runtime when a JavaScript challenge needs to be evaluated. It is always started with `--ignore-config`, so global or user yt-dlp configuration cannot silently change extraction behavior.

The default application settings are a 25-second request deadline and two concurrent yt-dlp processes. `WOTOHA_YTDLP_TIMEOUT_SECONDS` accepts 5–120 seconds and `WOTOHA_YTDLP_CONCURRENCY` accepts 1–8. Cookies are opt-in through `WOTOHA_YTDLP_COOKIES_FILE`; the path must be absolute, name an existing regular file, and have mode `0600` (or otherwise grant no group/other permissions).

The Ubuntu package installs verified copies under `/opt/wotoha/yt-dlp` and exposes the active version through `/opt/wotoha/bin/yt-dlp`. Releases are stored by SHA-256 digest with atomic `current` and `previous` links. yt-dlp metadata and checksums must come from an allowlisted official repository, the checksum signature must match the pinned full release-key fingerprint, and the Deno archive must match its pinned SHA-256 digest. The packaged `WOTOHA_YTDLP_PATH` and `WOTOHA_DENO_PATH` select these managed executables. An administrator may set absolute paths to different executables, but then owns their verification, updates, and compatibility; the independent updater continues maintaining the standard managed paths without overwriting the override.

`yt-dlp-update.timer` maintains this channel independently of Wotoha releases. A candidate must pass bounded extraction and direct-media-byte canaries before promotion; failure preserves the active version and state. Promotion does not restart the bot, so a healthy running application is not interrupted by an extractor-only update.

The retired native YouTube worker is documented only as migration history in [Ubuntu deployment](ubuntu-deploy.md). Fresh packages contain only the application plus the verified yt-dlp and Deno bundle.
