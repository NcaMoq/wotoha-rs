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
The first YouTube pass skips HLS and DASH manifest expansion with the official
yt-dlp extractor argument `youtube:skip=hls,dash`; this reduced extraction time
in the current 20-track playback probe while all returned sources remained
playable. If that pass returns no playable URL or yt-dlp explicitly reports
that the requested format is unavailable, Wotoha retries once with full format
discovery. It does not use this fallback for authentication, age, regional,
HTTP status, rate-limit, timeout, or other failures. The fallback is bounded to
one conservative extraction attempt.

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

## Resident Python API experiment

An opt-in comparison backend is available with
`WOTOHA_YTDLP_BACKEND=python-worker`; the default remains `cli`. The experiment
starts a bounded pool of long-lived Python workers, each with one sequential
`yt_dlp.YoutubeDL` instance. Rust sends newline-delimited JSON over stdin/stdout;
the pool permits concurrent work up to `WOTOHA_YTDLP_CONCURRENCY`. yt-dlp's
module and request-handler objects are retained between requests, while each
request still gets its own result and the same format fallback policy.

The OCI image supplies a hash-pinned Python environment and sets the interpreter
path and exact expected `yt_dlp.version.__version__`. The worker refuses a
version mismatch. Outside the image, administrators must install an exactly
matching package and set `WOTOHA_YTDLP_PYTHON_PATH` and
`WOTOHA_YTDLP_PYTHON_EXPECTED_VERSION`. It uses the same explicitly configured
cookie file and Deno path as the CLI; it does not import browser cookies or
bypass access restrictions. Worker stderr is discarded, protocol responses
are size-bounded, and only allowlisted generic error categories reach Rust.

This remains an opt-in research backend, not the production default. The
current OCI image includes the pinned interpreter and packages so the worker
can be tested reproducibly, but does not set `WOTOHA_YTDLP_BACKEND`; CLI
extraction remains authoritative by default. yt-dlp selects its own networking
handler. With the pinned Requests package installed, the tested build selected
yt-dlp's Requests handler. The application does not replace yt-dlp's HTTP
transport or cache signed media URLs.

`track_probe` reports worker startup, Rust↔Python IPC round-trip, and the
`extract_info` call separately, alongside total YouTube resolution and
Songbird preparation. The initial request includes cold worker startup in
`youtube_extract_ms`; later requests show the steady-state result.

The 2026-10-25 trial used the same 20 public video IDs in two sequential runs
per backend, in CLI → Python API → Python API → CLI order. Both paths used
yt-dlp `2026.07.23.234303`, Deno `2.9.4`, the same manifest-skip setting, the
same Rust probe binary, and no cookies. Each run used a fresh resolver/process;
there were no media cache hits. P95 is nearest-rank. Across 40 observations per
backend, CLI extraction median/P95 was `1965.92/2323.08 ms`; the Python worker
was `1197.19/1420.33 ms` (39.1% lower median and 38.9% lower P95). Total
extract-plus-Songbird verification was `1985.24/2341.52 ms` for CLI versus
`1209.42/1432.92 ms` for the worker. CLI extracted 40/40 sources and verified
39/40 playable; the one verification failure's HTTP class was not retained by
the benchmark's secret-safe output filter. The worker extracted and verified
40/40. In its instrumented repeat, every track used one API extraction and no
full-format fallback (0/20). Its per-track IPC median was 1192.39 ms versus an
`extract_info` median of 1190.64 ms, so local IPC overhead was about 1.75 ms;
the first request's cold worker startup added about 116 ms wall time. The
results supported keeping the worker opt-in, but did not switch the production
default. At the time of this initial comparison the image did not contain the
Python runtime/package, and that trial did not measure resident-worker memory
usage; the later image validation below covers both.

### Connection reuse and internal timing trial

A follow-up trial on 2026-10-25 used the same 20 public video IDs, yt-dlp
`2026.07.23.234303`, Deno `2.9.4`, no cookies, a fresh cache per run, and
Songbird input preparation for every resolved URL. The Python environment was
Python `3.12.3`, yt-dlp distribution `2026.7.23.234303.dev0` (module version
`2026.07.23.234303`), and Requests `2.34.2`. P95 is nearest-rank. The Requests
and urllib rows are serial Python-worker runs; the parallel row is a separate
two-worker run and reports per-track latency, not batch throughput.

| Worker networking mode | Extract median / P95 | Songbird playable | Decision |
| --- | ---: | ---: | --- |
| CLI (previous two-round reference) | 1965.92 / 2323.08 ms | 39/40 | Existing production default |
| urllib | 1161.91 / 1482.46 ms | 20/20 | Keep as compatible fallback |
| yt-dlp Requests handler | 1083.79 / 1425.15 ms | 20/20 | Prefer when already installed |
| Requests, 2 concurrent workers | 1142.93 / 1399.87 ms per track | 20/20 | Concurrency works; no per-track speedup shown |

The Requests run improved median extraction by 6.7% and P95 by 3.9% over the
urllib run, without reducing extraction or Songbird preparation success. In
the 20-track serial run, the two resident workers handled 60 HTTP requests with
two DNS lookups, two TCP connects, and two TLS handshakes total. The paired
urllib run needed 60 of each. This is direct evidence that yt-dlp's selected
Requests handler reused connections across requests; no custom transport or
vendor-specific extraction code was added. Requests-run HTTP open and body
read timings summed to 5.71 s and 15.25 s respectively across the 20
extractions. DNS, TCP, and TLS together were under 30 ms. No JavaScript solver
or runtime calls occurred. Thus the remaining wall time is overwhelmingly
upstream HTTP/request-response latency, not worker startup, DNS/TLS, or JS
execution. The profile is diagnostic: open/body intervals may overlap with
lower-level networking phases and must not be added to those phases as if
independent.

The worker stayed alive for all 20 sequential requests, with no CLI fallback.
An injected worker-process kill in a separate three-track check produced one
bounded CLI fallback; the following request started a replacement worker and
the remaining tracks prepared successfully. The two-worker 20-track run also
completed with no fallback. Measured maximum worker RSS was about 68 MiB
(child-process high-water about 58 MiB); summed worker and child CPU was about
2.30 s user+system for 20 extractions.

An additional trial of the official `youtube:player_skip=webpage` option
caused a public test video to return a sign-in/anti-bot classification and
made six requests instead of reducing them. It is therefore not enabled. The
existing `youtube:skip=hls,dash` optimization remains, and the Requests handler
is only an optional Python-environment optimization. A previous same-corpus
CLI reference run measured `1965.92/2323.08 ms` median/P95; this follow-up did
not rerun that slower CLI arm contemporaneously. The current worker median is
still above the aspirational 1000 ms target, so these results do not justify
changing the CLI default. The OCI image now contains the optional pinned worker
dependencies; it does not enable the worker automatically.

The trial uses yt-dlp's built-in networking handlers and leaves its format,
cookie, Deno, timeout, and access-control behavior in place. See the official
[embedding guide](https://github.com/yt-dlp/yt-dlp#embedding-yt-dlp),
[networking documentation](https://github.com/yt-dlp/yt-dlp#dependencies), and
[extractor arguments](https://github.com/yt-dlp/yt-dlp/blob/master/README.md#extractor-arguments).

### Matched CLI/Requests remeasurement and per-request timing

A focused 2026-10-25 rerun used the same 20 public video IDs twice per backend,
in CLI → Python worker → Python worker → CLI order. Each arm had a fresh
resolver process and cache, no cookies, identical pinned CLI/module release,
Deno `2.9.4`, `youtube:skip=hls,dash`, and the same release `track_probe`.
Every successful extraction was followed by Songbird input preparation. This
produced 40 observations per backend; P95 is nearest-rank.

| Backend | Extraction median / P95 | Total incl. Songbird median / P95 | Extracted | Songbird playable | Worker CLI fallback |
| --- | ---: | ---: | ---: | ---: | ---: |
| CLI (default) | 1918.77 / 2339.74 ms | 1931.92 / 2353.13 ms | 40/40 | 40/40 | — |
| Python API + Requests (opt-in) | 1184.91 / 1344.36 ms | 1199.67 / 1363.17 ms | 40/40 | 40/40 | 0 |

The worker reduced median extraction latency by 38.2% and P95 by 42.5% in this
matched run, while preserving successful Songbird preparation. It did not
reach the 1,000 ms median target: the result remained 184.91 ms above it.
Songbird readiness was small (13.53 ms median, 56.71 ms P95), so yt-dlp
extraction remains the bottleneck.

The Requests worker recorded exactly three HTTP requests per extraction. The
diagnostic records only route labels, status, and time—not URLs, paths, query
strings, or signed values.

| Ordinal | Route label | Status | `urlopen` median / P95 | Response body read median / P95 |
| ---: | --- | ---: | ---: | ---: |
| 1 | YouTube webpage | 200 | 103.50 / 135.19 ms | 799.85 / 951.83 ms |
| 2 | YouTube player API | 200 | 75.70 / 97.27 ms | 1.04 / 1.44 ms |
| 3 | YouTube player API | 200 | 103.40 / 128.45 ms | 1.09 / 1.69 ms |

Across the 40 extractions there were 120 HTTP requests, four DNS lookups, four
TCP connections, and four TLS handshakes total (the two pooled workers in each
of two fresh runs). No JavaScript solver/runtime calls occurred. Aggregate
`urlopen` time was 11.73 s and body-read time was 30.62 s; these are diagnostic
intervals and should not be added as independent phases. Worker CPU summed to
4.53 s user+system across the 40 extractions. Peak worker RSS was 68,428 KiB;
child-process high-water RSS was 60,384 KiB.

The first webpage response body is the dominant observed phase, with a median
read time near 800 ms. DNS/TCP/TLS reuse, JS work, local IPC, and Songbird are
not the remaining delay. The upstream `player_skip=webpage` trial is not safe:
it caused a sign-in/access classification and increased one measured
extraction to six requests. In the current three-request cases, no separate
client-config, initial-data API, or player-script request was observed; those
skip flags would not remove the large webpage body. Restricting player clients
could change available formats and access behavior, and was not adopted.
Therefore no additional network-skip setting was justified. The only retained
skip remains `youtube:skip=hls,dash`, and the Requests worker stays explicitly
opt-in.

### Packaged-image worker validation

On 2026-10-25, a local, unpublished image built from this worktree was tested
under the container's non-root, read-only, dropped-capability settings. The
same 20 public IDs were run once through each backend, with a fresh process and
empty writable tmpfs per arm, no cookies, the same Deno and yt-dlp release,
and Songbird preparation after every extraction. The CLI arm ran first. P95
uses nearest-rank on these 20 observations; this one-pass image check is kept
separate from the repeated 40-observation comparison above.

| Backend | Extract median / P95 | Total incl. Songbird median / P95 | Songbird playable | CLI fallback |
| --- | ---: | ---: | ---: | ---: |
| CLI (default) | 1900.45 / 2283.98 ms | 1954.63 / 2314.77 ms | 20/20 | — |
| Python API + Requests (opt-in) | 1220.69 / 1680.66 ms | 1270.55 / 1710.79 ms | 20/20 | 0 |

In this image run the worker median was 35.8% lower and P95 26.4% lower than
CLI. It still missed the 1000 ms target by 220.69 ms at the median. The three
Requests-handler calls were all HTTP 200 for all 20 tracks:

| Request | Open median / P95 | Body-read median / P95 |
| --- | ---: | ---: |
| YouTube watch page | 101.65 / 141.96 ms | 854.95 / 1154.47 ms |
| YouTube player API (first) | 73.30 / 95.61 ms | 0.90 / 1.35 ms |
| YouTube player API (second) | 105.64 / 135.66 ms | 1.02 / 1.60 ms |

The first webpage body remains the dominant cost; removing local process
startup, DNS/TLS setup, or the two tiny player-API bodies cannot plausibly
close the remaining gap. No new skip/client restriction was adopted because
the existing `player_skip=webpage` trial was incompatible with a public test
video and increased requests rather than reducing them.

The worker stayed alive for all 20 serial tracks with zero fallbacks. An
8-track, 4-way parallel image run completed 8/8 with zero CLI fallbacks. In a
separate 3-track crash-injection run, killing the sole worker caused exactly
one bounded CLI fallback; a replacement worker handled the final track, and
all three remained playable. Across the 20-track worker arm, measured worker
CPU summed to 2.30 seconds user+system, peak worker RSS was 82,500 KiB (80.6
MiB), and child high-water RSS was 66,908 KiB (65.3 MiB).

The image build also verified exact Python yt-dlp distribution/module pins,
Requests `2.34.2`, selection of yt-dlp's Requests handler, and parsing of a
synthetic Netscape-format cookie fixture with restrictive file permissions.
No real account cookie was available or used, so authenticated playback is
not claimed as tested. The cookie path remains the same explicit,
administrator-configured path used by the CLI. Python dependencies are
SHA-256-pinned, their license texts are present in the image, and the venv is
read-only; `-B` prevents bytecode writes. The image does not set
`WOTOHA_YTDLP_BACKEND`, so CLI remains the default.

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
