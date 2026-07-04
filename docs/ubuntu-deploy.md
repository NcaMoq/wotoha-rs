# Ubuntu Server 導入手順

この手順は `x86_64` の Ubuntu Server を対象にしています。
配布物は `x86_64-unknown-linux-musl` で作成した静的バイナリです。

## 1. Windows 側で Ubuntu 用の配布物を作成する

リポジトリ直下で次を実行します。

```powershell
powershell -ExecutionPolicy Bypass -File .\deploy\build-ubuntu-musl.ps1
```

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
sudo apt install -y ca-certificates curl jq tar util-linux
cd /tmp
tar -xzf wotoha-ubuntu-x86_64-musl.tar.gz
cd wotoha-ubuntu-x86_64-musl
sudo bash ./install-ubuntu.sh
```

次の場所へ配置されます。

- `/opt/wotoha/bin/wotoha-app`
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
ls -lh /opt/wotoha/bin/wotoha-app
sha256sum /opt/wotoha/bin/wotoha-app
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

GitHubで `v` から始まるタグ（例: `v0.2.0`）をpushすると、ReleaseワークフローがUbuntu用配布物をビルドして公開します。自動更新はドラフトとプレリリースを対象にしません。

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
sudo rm -f /etc/systemd/system/wotoha.service
sudo rm -f /etc/systemd/system/wotoha-update.service
sudo rm -f /etc/systemd/system/wotoha-update.timer
sudo rm -rf /opt/wotoha
sudo rm -rf /etc/wotoha
sudo rm -rf /var/lib/wotoha
sudo rm -rf /var/lib/wotoha-updater
sudo rm -rf /var/log/wotoha
sudo systemctl daemon-reload
```
