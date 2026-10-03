# Privacy

Wotoha is a self-hosted application. The operator controls the host, the
container, credentials, logs, and persistent data.

## Data handled at runtime

- Discord identifiers needed to authorize commands and maintain guild voice
  state.
- User-submitted track URLs and provider metadata needed to resolve and play a
  request.
- Playback queue, reconnect state, and bounded analysis cache entries.
- Operational logs and bounded container stdout/stderr logs.

The application does not intentionally collect analytics or sell personal
information. Network requests are limited to Discord, supported media
providers, and the pinned media helper processes required for playback.

## Retention and deletion

The operator should set Docker log rotation and restrict `runtime.env` to the
service account. Persistent state lives under the host data directory mounted
at `/wotoha`; deleting its cache, logs, reconnect state, and tool override
files removes those locally retained records. Discord and provider-side
retention is governed by their respective services and operator settings.

## Access requests and questions

For questions about a particular deployment, contact the operator who runs
that deployment. For project questions, open a public issue without including
tokens, raw private observations, exported media, or personal data.
