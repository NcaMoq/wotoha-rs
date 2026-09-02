# AutoMix real-corpus evaluator

This directory stores URLs and expectations only. The evaluator never writes downloaded source
media into the repository. Analysis caches, rendered WAV previews, and the JSON report are written
under a newly-created directory in the operating system's temporary directory.
Downloaded (including copyrighted) audio is kept only under that OS-temporary run directory and
is removed by default.
The release gate requires at least 8 matched kick detections and 65% kick coverage
(`min_matched_kicks: 8`, `min_kick_coverage: 0.65`) in each rendered overlap. Coverage is
matched kicks divided by expected beats in the rendered overlap for which both source analyses
provide beat grids; it is not a whole-song estimate. A missing/empty pair of source grids fails
closed with an explicit `kick_expected_beats_unavailable` issue.
Each fixture also declares its expected YouTube video ID and title identity tokens. Resolution
must produce the same canonical ID and every normalized token (case, punctuation, and Unicode
separator differences are ignored), including the artist/title and `Extended Mix` identity; a
missing token or different video is an acquisition failure. The report records the resolved title,
video ID, and identity failure code.
Analysis reports include the typed backend (`Neural`, `ClassicalPermanentIneligible`,
`ClassicalTransientFailure`, `CachedNeural`, `CachedClassicalPermanentIneligible`, or
`CachedClassicalTransientFailure`) and reason.
The strict live gate accepts only fresh `Neural` analysis for every fixture; cache hits and
classical fallback metrics cannot produce a pass.
Any inherited `WOTOHA_ANALYSIS_CACHE_DIR` is ignored for the run and restored afterward; the
runner always selects `<unique-temp-run>/.wotoha-analysis`.

Each fixture emits begin/end progress events to stderr using only its manifest ID. Conservative
absolute stage deadlines are: resolve 90s, prepare 90s, content hashing 180s, analysis 300s,
and pair rendering 180s. A deadline produces a distinct timeout attempt code (for example
`resolve_timeout`, `hash_timeout`, `analyze_timeout`, or `render_timeout`), continues with the
remaining fixtures/pairs, and leaves the final gate failed. Hashing carries one absolute deadline
through all ranged requests and response-body reads, so per-request retries cannot multiply the
stage budget.
If a ranged request receives a complete `200`, it is accepted only when a positive bounded
`Content-Length`, an audio/video (or octet-stream) media type, and an exactly-sized body all
agree with the resolver's known length. It is hashed once as the complete representation; a
missing length, error/text type, partial body, or mismatched length is rejected. A non-HTTPS
redirect is rejected in production (localhost HTTP is used only by unit tests).
Provider acquisition failures such as 403/410 and transient range failures trigger at most two
fresh request refreshes, each repeating resolve, identity validation, prepare, hash, and analysis;
retry cycles bypass the prior canonical-key analysis cache so their backend remains an actual
fresh attempt. The same signed URL is never retried for provider expiry responses. A fixture has
a 900s absolute cap across those cycles, and
the report records each cycle and `request_refresh` marker. A failed hash may still feed
diagnostic planning/rendering, but it always keeps the strict fixture gate false.
The runtime analysis and preview APIs signal cancellation on future drop. Preview source readers
and the CPU renderer share that cancellation contract, so the runner's outer 180s pair-rendering
deadline cancels both source preparation/reads and the blocking decode/DSP/WAV worker. The worker
cooperatively checks cancellation at packet, frame, and encoding boundaries and never uses unsafe
thread killing; bounded HTTP connect/read timeouts cover a source already inside a network read.

Network execution is deliberately double opt-in:

```powershell
$env:WOTOHA_AUTOMIX_CORPUS_ALLOW_NETWORK = '1'
$env:WOTOHA_YTDLP_PATH = (Get-Command yt-dlp).Source
cargo run -p wotoha-app --bin automix_corpus -- --allow-network
```

On deployments that use a separate JavaScript runtime, configure `WOTOHA_DENO_PATH` as usual.
Use `--manifest <absolute-or-relative-path>` to select another URL-only corpus and `--report
<path>` to copy the final JSON report outside the temporary run directory.
By default the temporary run directory (downloads, cache, previews, and intermediate data) is
removed on both success and failure. Pass `--keep-artifacts` when investigating a run; cleanup
only accepts the exact uniquely-created child of the OS temp directory. If cleanup itself fails,
the evaluator emits a warning without replacing the primary gate result.

The process emits the report even when a source cannot be acquired or a pair fails its thresholds,
then exits non-zero. The default workspace test suite never invokes the live network path; unit
tests use generated PCM and localhost-only HTTP fixtures.
