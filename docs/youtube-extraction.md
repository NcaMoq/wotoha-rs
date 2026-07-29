# YouTube extraction

Wotoha keeps YouTube-specific change points outside the playback engine:

1. Rust selects an Innertube client strategy and creates a visitor-bound Player request.
2. Every returned GVS or HLS source is verified by reading real response data.
3. A configured PO Token provider is tried only after a `401` or `403`.
4. `yt-dlp` remains an optional emergency fallback, not the primary extractor.

Native client profiles live in `/etc/wotoha/youtube-clients.json` and reload without restarting
the bot. Profile changes alone therefore do not require a binary release.

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

## Remaining independently updatable boundary

Cipher-only formats still require Player JavaScript challenge solving. The intended boundary is a
versioned bulk protocol that receives the Player URL/hash plus `signatureCipher` and `n` inputs,
then returns solved values. The Rust extractor remains responsible for strategy selection,
fallback, caching, URL assembly and final data verification. Solver logic should be shipped as a
signed, independently updatable WASM component so a YouTube Player change does not require
rebuilding the bot.
