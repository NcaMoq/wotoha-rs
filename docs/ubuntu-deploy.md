# Ubuntu Server 導入手順

この手順は `x86_64` の Ubuntu Server を対象にしています。
配布物は `x86_64-unknown-linux-musl` で作成した静的バイナリです。

## 1. Windows 側で Ubuntu 用の配布物を作成する

リポジトリ直下で次を実行します。

```powershell
powershell -ExecutionPolicy Bypass -File .\deploy\build-ubuntu-musl.ps1
```

The manual packager also requires `curl.exe` and GPG. It detects `gpg.exe` on `PATH` or the GPG
executable bundled with a standard Git for Windows installation. The script verifies the same
official yt-dlp signature, full key fingerprint, and pinned Deno checksum as release CI.

次の成果物が作成されます。

- `target\ubuntu-musl\x86_64-unknown-linux-musl\release\wotoha-app`
- `dist\wotoha-ubuntu-x86_64-musl\`
- `dist\wotoha-ubuntu-x86_64-musl.tar.gz`

初回は構築用の道具を入れます。

```powershell
cargo install cargo-zigbuild
winget install --id zig.zig -e --accept-source-agreements --accept-package-agreements
winget install --id Kitware.CMake -e --accept-source-agreements --accept-package-agreements
winget install --id Ninja-build.Ninja -e --accept-source-agreements --accept-package-agreements
rustup target add x86_64-unknown-linux-musl
```

## 2. 配布物を Ubuntu Server へ転送する

Windows 側で配布アーカイブを転送します。

```powershell
scp .\dist\wotoha-ubuntu-x86_64-musl.tar.gz user@your-server:/tmp/
```

## 3. Ubuntu Server で展開して導入する

Ubuntu 側で次を実行します。

```bash
sudo apt update
sudo apt install -y ca-certificates coreutils curl gnupg jq tar util-linux
cd /tmp
tar -xzf wotoha-ubuntu-x86_64-musl.tar.gz
cd wotoha-ubuntu-x86_64-musl
sudo bash ./install-ubuntu.sh
```

次の場所へ配置されます。

- `/opt/wotoha/bin/wotoha-app`
- `/opt/wotoha/bin/yt-dlp`
- `/opt/wotoha/bin/deno`
- `/etc/systemd/system/wotoha.service`
- `/etc/wotoha/wotoha.env`
- `/var/lib/wotoha`
- `/var/log/wotoha`

## 4. 環境変数を設定する

次のファイルを編集します。

```bash
sudoedit /etc/wotoha/wotoha.env
```

設定例です。

```dotenv
DISCORD_TOKEN=xxxxxxxxxxxxxxxx
RUST_LOG=info,wotoha_debug=info
WOTOHA_LOG_DIR=/var/log/wotoha
WOTOHA_LOG_FILE=wotoha-app.runtime.log
WOTOHA_LOG_ANSI=false
WOTOHA_DEFAULT_VOLUME=0.10
WOTOHA_MAX_QUEUE_LEN=512
WOTOHA_MAX_PENDING_ENQUEUES=64
```

起動時に設定値を検査します。`WOTOHA_LOG_FILE` は `WOTOHA_LOG_DIR` 配下に作成するファイル名です。`/` と `\` を含む値は拒否されます。

`WOTOHA_DEFAULT_VOLUME` は `0.0..=2.0`、`WOTOHA_MAX_QUEUE_LEN` は `1..=512`、`WOTOHA_MAX_PENDING_ENQUEUES` は `1..=64` を受け付けます。`WOTOHA_MAX_PENDING_ENQUEUES` は `WOTOHA_MAX_QUEUE_LEN` 以下にしてください。

## 5. サービスを起動する

```bash
sudo systemctl daemon-reload
sudo systemctl enable --now wotoha.service
sudo systemctl status wotoha.service
```

## 6. 動作を確認する

サービス状態を確認します。

```bash
systemctl status wotoha.service --no-pager
```

記録を確認します。

```bash
journalctl -u wotoha.service -f
tail -f /var/log/wotoha/wotoha-app.runtime.log
```

バイナリと検査用摘要値を確認します。

```bash
ls -lh /opt/wotoha/bin/wotoha-app /opt/wotoha/bin/yt-dlp /opt/wotoha/bin/deno
sha256sum /opt/wotoha/bin/wotoha-app /opt/wotoha/bin/yt-dlp /opt/wotoha/bin/deno
cat /tmp/wotoha-ubuntu-x86_64-musl/SHA256SUMS.txt
```

## 7. 自動更新する

インストーラーは `wotoha-update.timer` を有効化します。15分間隔（最大2分のランダム遅延付き）でGitHub Releasesを確認し、新しい正式リリースがあれば次の処理を行います。

1. 配布アーカイブとSHA-256ファイルをダウンロード
2. アーカイブとバイナリのSHA-256を検証
3. バイナリを原子的に差し替え
4. Botが実行中だった場合だけ再起動
5. 起動に失敗した場合は直前のバイナリへロールバック

状態とログは次のコマンドで確認できます。

```bash
systemctl status wotoha-update.timer --no-pager
journalctl -u wotoha-update.service
sudo systemctl start wotoha-update.service
```

既存の手動ビルドを初めて自動更新の管理下へ移す場合、現在の正式リリースを基準として記録し、意図しないダウングレードを防ぎます。すぐ正式リリースへ置き換える場合だけ次を実行してください。

```bash
sudo /opt/wotoha/bin/wotoha-update --force
```

更新元は `/etc/wotoha/wotoha-update.env` で設定します。GitHubリポジトリを移動した場合だけ変更してください。

```dotenv
WOTOHA_UPDATE_REPOSITORY=NcaMoq/wotoha-rs
WOTOHA_UPDATE_GITHUB_TOKEN=github_pat_xxxxxxxxxxxx
```

Private Repositoryでは、対象リポジトリの `Contents: Read-only` 権限だけを持つfine-grained personal access tokenを設定してください。このファイルはrootだけが読めるモードで作成されます。Public Repositoryではトークンを空にできます。

GitHubで `v` から始まるタグ（例: `v0.2.0`）をpushすると、ReleaseワークフローがUbuntu用配布物をビルドして公開します。自動更新はドラフトとプレリリースを対象にしません。配布archiveとmanifestは両方ともGitHub Artifact Attestationで署名され、更新前にrepository、workflow、tag、commit、GitHub-hosted runnerまで検証されます。このため通常の自動更新にもGitHub公式配布の新しい`gh`が必要です。

### 手動で更新する

Windows 側で新しい配布物を作成して再転送した後、Ubuntu 側で次を実行します。

```bash
sudo systemctl stop wotoha.service
cd /tmp
rm -rf wotoha-ubuntu-x86_64-musl
tar -xzf wotoha-ubuntu-x86_64-musl.tar.gz
cd wotoha-ubuntu-x86_64-musl
sudo bash ./install-ubuntu.sh
sudo systemctl restart wotoha.service
```

`/etc/wotoha/wotoha.env` は残るため、更新のたびに `DISCORD_TOKEN` を入れ直す必要はありません。

## 8. 削除する

```bash
sudo systemctl disable --now wotoha.service
sudo systemctl disable --now wotoha-update.timer
sudo systemctl disable --now yt-dlp-update.timer
sudo rm -f /etc/systemd/system/wotoha.service
sudo rm -f /etc/systemd/system/wotoha-update.service
sudo rm -f /etc/systemd/system/wotoha-update.timer
sudo rm -f /etc/systemd/system/yt-dlp-update.service
sudo rm -f /etc/systemd/system/yt-dlp-update.timer
sudo rm -rf /opt/wotoha
sudo rm -rf /etc/wotoha
sudo rm -rf /var/lib/wotoha
sudo rm -rf /var/lib/wotoha-updater
sudo rm -rf /var/log/wotoha
sudo systemctl daemon-reload
```

## Phase A bridge from v0.5.29 and Phase B finalization

The first migration release is intentionally a bridge release. It must retain the existing
`wotoha-app`, `wotoha-youtube-js-worker`, `deploy/YOUTUBE_WORKER_SEQUENCE`, and
`deploy/youtube-clients.json` package contract so that the updater installed by v0.5.29 can verify
and apply it. Do not remove the worker files or worker state during Phase A. The bridge advances
`YOUTUBE_WORKER_SEQUENCE` from production sequence 1 to sequence 2, allowing the v0.5.29 updater to
accept a reproducibly rebuilt worker even when its digest differs from the installed sequence-1
binary.

The v0.5.29 updater first installs the bridge application and the new general
`/opt/wotoha/bin/wotoha-update`. On its next run, the new updater downloads and verifies the same
GitHub-attested archive again because the managed yt-dlp installation is not ready yet. Before any
application replacement or restart, it then:

1. verifies the package checksums;
2. imports the checked-in yt-dlp release key and requires the complete fingerprint
   `AC0CBBE6848D6A873464AF4E57CF65933B5A7581`;
3. verifies `SHA2-256SUMS.sig` and the `yt-dlp_linux` digest;
4. checks the pinned Deno digest;
5. runs yt-dlp with Deno and tries the two pinned extraction/direct-byte canaries; and
6. promotes `/opt/wotoha/yt-dlp/current` atomically only after a canary succeeds.

The compatibility path `/opt/wotoha/bin/yt-dlp` is a symlink to the managed current version, so an
existing v0.5.29 `WOTOHA_YTDLP_PATH=/opt/wotoha/bin/yt-dlp` setting remains valid. Custom values in
`/etc/wotoha/wotoha.env` are not overwritten.

The application always invokes yt-dlp with `--ignore-config`. Tune its bounded provider directly in
`/etc/wotoha/wotoha.env`: `WOTOHA_YTDLP_TIMEOUT_SECONDS` accepts 5–120 seconds (default 25), and
`WOTOHA_YTDLP_CONCURRENCY` accepts 1–8 processes (default 2). Optional
`WOTOHA_YTDLP_COOKIES_FILE` must be an absolute path to an existing regular file with mode `0600`
(or stricter). The packaged yt-dlp and Deno paths are recommended; absolute administrator overrides
are accepted but place verification, updates, and compatibility under the administrator's control.

yt-dlp updates are independent of bot releases and bot restarts:

```bash
systemctl status yt-dlp-update.timer --no-pager
sudo systemctl start yt-dlp-update.service
journalctl -u yt-dlp-update.service --since today
readlink -f /opt/wotoha/yt-dlp/current
/opt/wotoha/bin/yt-dlp --version
/opt/wotoha/bin/deno --version
```

The timer checks every six hours (with a randomized delay). It follows the official
`yt-dlp/yt-dlp-nightly-builds` channel by default because upstream recommends nightly builds for
regular users and YouTube compatibility changes frequently. The only accepted repositories are
`yt-dlp/yt-dlp-nightly-builds` and the official stable `yt-dlp/yt-dlp` repository.

Every updater yt-dlp subprocess is bounded with GNU `timeout`: version checks have 20 seconds and each
extraction canary has 60 seconds. Canary network calls use a 10-second socket timeout with two
download retries and two extractor retries. The independent systemd job has a five-minute start
limit; the general signed-release updater, which also performs first-time yt-dlp/Deno bootstrap,
has a 15-minute limit. A timeout fails closed without changing the active yt-dlp pointer or state.
All updater HTTP GETs retry transient errors (including connection and TLS resets) with a 10-second
connect timeout, fixed delay, and a 45-second retry window; failed file downloads are removed before
the transaction exits.

Configure the independent updater in `/etc/wotoha/wotoha-update.env`. Quote multiple canary URLs
because this file is sourced by Bash:

```dotenv
WOTOHA_UPDATE_YTDLP=true
WOTOHA_YTDLP_UPDATE_REPOSITORY=yt-dlp/yt-dlp-nightly-builds
WOTOHA_YTDLP_CANARY_URLS='https://www.youtube.com/watch?v=H7HmzwI67ec https://www.youtube.com/watch?v=jNQXAC9IVRw'
```

Only after the Phase A release has been observed with a valid `current` pointer, working Deno, and
a successful `yt-dlp-update.service` run does Phase B ship its worker-less application package. The
Phase A updater applies that signed package while temporarily retaining the installed worker. The
new Phase B updater removes the legacy worker on its next same-tag pass, only after the final app
release is recorded, managed yt-dlp state and digest still validate, and `wotoha.service` is not
failed. If the service was active, the prior updater transaction recorded the final tag and app
digest only after its restart stayed active. An inactive service is treated as intentional operator
state after the signed binary and digest checks pass. Cleanup is idempotent and does not restart an
active or intentionally inactive yt-dlp-only app.

The cleanup removes only the known native YouTube worker variables, binary, content-addressed
worker directory, pointers, ACK/state files, sequence file, and `youtube-clients.json`. It does not
remove `/etc/wotoha/yt-dlp.conf`, cookies, custom `WOTOHA_YTDLP_*` values, or other operator data.
