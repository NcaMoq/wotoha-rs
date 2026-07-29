# YouTube extraction

Wotoha keeps YouTube-specific change points outside the playback engine:

1. Rust selects an Innertube client strategy and creates a visitor-bound Player request.
2. Cipher-only formats are solved natively: OXC finds challenge functions structurally in the
   Player JavaScript AST and Boa executes only the prepared challenge slice.
3. Every returned GVS or HLS source is verified by reading real response data.
4. On a failed direct URL, Wotoha solves its `n` challenge and verifies it again.
5. A configured PO Token provider is tried only after a `401` or `403`.
6. `yt-dlp` remains an optional emergency fallback, not the primary extractor.

Native client profiles live in `/etc/wotoha/youtube-clients.json` and reload without restarting
the bot. Profile changes alone therefore do not require a binary release.

Player scripts are limited to 8 MiB, restricted to YouTube HTTPS origins and cached by URL.
Wotoha sends length-prefixed, versioned requests to the separate
`wotoha-youtube-js-worker` Rust process. That process owns OXC parsing and Boa execution, retains
up to four Player sessions, and receives only small signature/`n` batches after the first request
for a Player version. The parent applies queue and execution deadlines, kills a stuck worker,
backs off the failing Player fingerprint, and starts a fresh process on a later request. On Linux
the worker also limits its own address space and dies with its parent. A Player parser/runtime
panic therefore cannot directly abort the Discord bot process.

Release packages contain a standalone worker. For compatibility with an older updater that only
installs `wotoha-app`, the app can also re-exec itself in worker mode; Player code is still handled
in a child process rather than inside the bot process.

The ordinary direct-URL path does not start the solver unless its URL carries an `n` challenge.
Cipher candidates are kept as a verified fallback if a direct or HLS candidate is rejected.

GitHub Actions runs deterministic workspace and worker-protocol tests on every pull request and
push to `main`. A separate daily canary downloads the current Player, checks historical official
EJS answer vectors, verifies worker restart behavior, and requires a real YouTube track probe to
report `PLAYABLE`. Live-network failures alert without making ordinary pull requests flaky.

## PO Token provider protocol

Set `WOTOHA_YOUTUBE_PO_TOKEN_PROVIDER` to an absolute executable path. Wotoha starts the
executable directly without a shell, sends one JSON object to stdin, and expects one JSON object
on stdout. The provider receives only a small environment allowlist; bot credentials are not
inherited.

Request:

```json
{
  "protocol_version": 1,
  "client": {
    "profile_id": "android_vr",
    "client_name": "ANDROID_VR",
    "client_version": "1.65.10",
    "client_number": "28",
    "user_agent": "com.google.android.apps.youtube.vr.oculus/...",
    "os_name": "Android",
    "os_version": "12L",
    "device_make": "Oculus",
    "device_model": "Quest 3",
    "android_sdk_version": 32
  },
  "context": "gvs",
  "video_id": "video-id",
  "visitor_data": "visitor-data"
}
```

`context` is either `player` or `gvs`.

Response:

```json
{
  "protocol_version": 1,
  "token": "opaque-token",
  "expires_in_seconds": 600
}
```

Return `{"protocol_version": 1, "token": null}` when the provider cannot produce a token. The
process must write no secrets to stderr. Wotoha limits execution time and output size, caches
tokens by client, context, video and visitor session, and temporarily backs off after provider
failures.

For HTTPS GVS sources the token is added as the `pot` query parameter. For HLS manifests it is
added as `/pot/{token}`. The PO Token expiry shortens the prepared-source cache lifetime.

## Update boundaries

Client profiles and the PO Token provider can be updated independently of the bot. The native
Player solver is intentionally generic and follows structural markers instead of minified symbol
names. Its AST discovery logic is compiled into the separately packaged worker executable, so a
normal release can replace that boundary together with the bot while keeping crashes isolated. A
future signed worker-only update channel can decouple its cadence further if Player changes prove
to require it.
