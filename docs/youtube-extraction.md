# YouTube extraction

Production containers use the immutable image-pinned fallback at
`/app/tools/yt-dlp-fallback` and `/app/tools/deno`. The application image is
immutable and does not self-update. A separately managed optional yt-dlp
override may be placed at `/wotoha/tools/yt-dlp` only when its
`/wotoha/tools/yt-dlp.sha256` sidecar matches the executable. The independent
yt-dlp updater remains a migration-only concern for legacy native hosts.

The yt-dlp resolution order is:

1. `WOTOHA_YTDLP_PATH`: an explicit administrator override, which must be an
   absolute path.
2. The verified managed override `/wotoha/tools/yt-dlp`.
3. The immutable image fallback `/app/tools/yt-dlp-fallback`.
4. The legacy `/opt/wotoha/bin/yt-dlp` path, only when retained for migration.

If the managed executable is present but its sidecar is missing or wrong,
Wotoha emits a diagnostic, refuses that executable, and continues with the
immutable image fallback. The explicit environment override is not silently
replaced: administrators selecting it own its availability and verification.

Wotoha resolves YouTube tracks by starting the official `yt-dlp` executable for each request. `yt-dlp` uses the separately installed Deno runtime when a JavaScript challenge needs to be evaluated. It is always started with `--ignore-config`, so global or user yt-dlp configuration cannot silently change extraction behavior.

The extractor command is intentionally small and explicit: one URL, one JSON
result, no playlist expansion, bounded retries, and a bounded stderr capture.
Wotoha does not bypass account, age, regional, or anti-bot controls. A cookie
file is used only when an administrator explicitly configures
`WOTOHA_YTDLP_COOKIES_FILE` for content that administrator is authorized to
access. It must be an absolute regular file with no group/other permissions
(normally mode `0600`), and its contents are never logged or sent to Discord.

The current yt-dlp guidance requires cookies in Mozilla/Netscape format when a
cookie file is needed, and recommends an external JavaScript runtime for the
YouTube challenge path. See the [yt-dlp FAQ](https://github.com/yt-dlp/yt-dlp/wiki/FAQ)
and [EJS runtime guidance](https://github.com/yt-dlp/yt-dlp/wiki/EJS) for the
upstream format and compatibility requirements. Wotoha passes the configured
Deno executable as `--js-runtimes deno:<absolute-path>`; it does not download
or update a runtime while handling a user request.

The default application settings are a 25-second request deadline and two concurrent yt-dlp processes. `WOTOHA_YTDLP_TIMEOUT_SECONDS` accepts 5–120 seconds and `WOTOHA_YTDLP_CONCURRENCY` accepts 1–8. Cookies are opt-in through `WOTOHA_YTDLP_COOKIES_FILE`; the path must be absolute, name an existing regular file, and have mode `0600` (or otherwise grant no group/other permissions).

## Playback recovery and error categories

Resolved media URLs can expire between extraction and decoder startup. The
runtime preserves provider URL and redirect validation, headers, and range
response checks. When the lazy audio source reports a typed recoverable HTTP
status such as `401`, `403`, `410`, or a transient server status, Wotoha
invalidates the prepared-source cache and asks the provider for one fresh
source. The retry is bounded to one attempt, keeps the same logical playback
operation, and refuses to retry when the refreshed source is identical. `404`
and malformed range responses are not treated as refreshable source expiry.

This path also applies to an AutoMix deck while it is being prepared. A failed
prefetch is never promoted as a BeatMatched deck, and an expired or rejected
source cannot be reused from the normal prepared cache. Recovery of an already
audible stream is intentionally not attempted: restarting it without a
position guarantee could duplicate audio or break the transition timeline.

yt-dlp extraction failures are classified separately. Age verification,
authentication-required, access-restricted, and temporary extraction failures
produce short user-facing messages; raw yt-dlp stderr, cookies, authorization
headers, and signed media URLs are not sent to Discord. An age/authentication
message means the administrator must configure a valid authorized source or
choose another track; Wotoha does not implement a restriction bypass.

The release archives do not redistribute yt-dlp or Deno. The updater-compatible installer downloads the pinned releases directly from their official GitHub repositories during installation, then installs verified copies under `/opt/wotoha/yt-dlp` and exposes the active version through `/opt/wotoha/bin/yt-dlp`. Releases are stored by SHA-256 digest with atomic `current` and `previous` links. yt-dlp metadata and checksums must come from an allowlisted official repository, the checksum signature must match the pinned full release-key fingerprint, and the Deno archive must match its pinned SHA-256 digest. The installer also verifies the reported versions and extraction/direct-media-byte canaries before promotion. `WOTOHA_YTDLP_PATH` is reserved for an explicit administrator override; `WOTOHA_DENO_PATH` selects the Deno executable. An administrator may set absolute paths to different executables, but then owns their verification, updates, and compatibility; the independent updater continues maintaining the standard managed paths without overwriting the override.

`yt-dlp-update.timer` maintains this channel independently of Wotoha releases. A candidate must pass bounded extraction and direct-media-byte canaries before promotion; failure preserves the active version and state. Promotion does not restart the bot, so a healthy running application is not interrupted by an extractor-only update.

The retired native YouTube worker is documented only as migration history in [Ubuntu deployment](ubuntu-deploy.md). Fresh archives contain the application and bootstrap scripts, not yt-dlp or Deno binaries.
