# Wotoha RS — AutoMix対応のRust製Discord音楽Bot

日本語 | [English](README.md)

[![最新リリース](https://img.shields.io/github/v/release/NcaMoq/wotoha-rs?sort=semver&label=release)](https://github.com/NcaMoq/wotoha-rs/releases/latest)
[![CI](https://github.com/NcaMoq/wotoha-rs/actions/workflows/ci.yml/badge.svg)](https://github.com/NcaMoq/wotoha-rs/actions/workflows/ci.yml)
[![Rust](https://img.shields.io/badge/Rust-2024-000000?logo=rust)](https://www.rust-lang.org/)

<p align="center">
  <a href="https://discord.com/oauth2/authorize?client_id=1238488423208063107"><img src="https://img.shields.io/badge/Wotoha%E3%82%92Discord%E3%81%AB%E8%BF%BD%E5%8A%A0-5865F2?style=for-the-badge&logo=discord&logoColor=white" alt="WotohaをDiscordに追加"></a>
</p>

**Wotoha RS** は、Discord音楽BOT **[Wotoha](https://github.com/NcaMoq/wotoha)** をベースに、Codexを使用してRustで再設計・再実装したものです。

DiscordのスラッシュコマンドからURLを指定することで、ボイスチャンネル内で音楽を再生できます。YouTube、SoundCloud、Bandcamp、ニコニコ動画、Vimeo、Twitch、Xに対応しています。

基本的には、上のボタンからWotohaをDiscordサーバーに追加して使います。自分でサーバーを用意する必要はありません。

> 気に入ったら[GitHubでStarを付けて](https://github.com/NcaMoq/wotoha-rs)もらえると嬉しいです。

## WotohaをDiscordに追加

通常は次の手順ですぐに使えます。

1. [Wotohaの招待ページを開きます](https://discord.com/oauth2/authorize?client_id=1238488423208063107)。
2. Wotohaを追加するDiscordサーバーを選んで認証します。
3. ボイスチャンネルに参加し、対応している音楽URLを `/play` で指定します。

## Wotoha RSの特長

- **適応型AutoMix** — BPM、ビート信頼度、曲構造、音圧、ボーカル、調性の相性を解析してから曲間のつなぎ方を決定します。
- **安全なフォールバック** — 条件が良ければBeatMatched Mix、難しい場合は適応型Crossfade、重ねると不自然になる場合はGaplessへ自動移行します。
- **曲ごとの音量を均一化** — 既定で `-16 LUFS` を目標に正規化し、`-2 dBTP` のTrue Peak上限とブースト量制限で過大な増幅を防ぎます。
- **複数サービスに対応** — YouTube、SoundCloud、Bandcamp、ニコニコ動画、Vimeo、Twitchの配信・VOD、XのメディアURLを扱えます。
- **シンプルなDiscord操作** — `/play <url>` で曲を追加し、Skip、Loop、Shuffle、AutoMix、Listボタンから操作できます。
- **サーバーの準備は不要** — WotohaをDiscordへ追加するだけで利用できます。
- **Rustの音声処理スタック** — Tokio、Serenity、Songbird、Symphoniaを使用したモジュール構成のCargo workspaceです。

## AutoMixの仕組み

Wotoha RSは曲の切り替え前に、再生中の曲と次の曲を解析します。使用可能なイントロ・アウトロ、テンポの相性、ビートとフレーズの位置、ボーカルの重なり、音圧の連続性、ピークの余裕を評価します。

解析結果から、安全な方式を順に選びます。

1. **BeatMatched** — 相性の良い曲をテンポ調整し、ビートを合わせてミックスします。
2. **Crossfade** — ビート合わせの信頼性が不足する場合に、曲に合わせたクロスフェードを行います。
3. **Gapless** — 曲を重ねると品質が下がる場合に、重なりのない切り替えを行います。

品質ガードが位相ずれ、ボーカル同士の衝突、クリッピングの危険、深い音圧低下を検出し、不自然なMixを拒否します。ラウドネス正規化は曲ごとに一度だけ適用され、その後に最終Peak Guardが働きます。

## 対応する音楽・メディアサービス

| サービス | 対応内容 |
| --- | --- |
| YouTube | 管理されたyt-dlpによる動画・音楽URLの再生 |
| SoundCloud | 公開トラック |
| Bandcamp | 公開トラックページ |
| ニコニコ動画 | 公開動画URL |
| Vimeo | 公開動画 |
| Twitch | ライブ配信とVOD |
| X / Twitter | 再生可能なメディアを含む投稿 |

各サービスの公開仕様が変わると、一時的に再生できなくなる場合があります。

## Discordでの使い方

ボイスチャンネルに参加して、次のコマンドを実行します。

```text
/play url:https://example.com/music
```

再生メッセージには次の操作ボタンが表示されます。

| ボタン | 動作 |
| --- | --- |
| **Skip** | 再生中の曲をスキップ |
| **Loop** | 再生中の曲のループを切り替え |
| **Shuffle** | キュー内の曲順をシャッフル |
| **AutoMix** | DJ風の自動トランジションを切り替え |
| **List** | 再生中の曲とキューの一覧を表示 |

## 設定

ローカル開発時は `.env`、Linux向けパッケージでは `/etc/wotoha/wotoha.env` から設定を読み込みます。

| 環境変数 | 既定値 | 用途 |
| --- | ---: | --- |
| `DISCORD_TOKEN` | 必須 | Discord Botトークン |
| `WOTOHA_DEFAULT_VOLUME` | `0.10` | マスター再生音量 |
| `WOTOHA_AUTOMIX_ENABLED` | `true` | 起動時からAutoMixを有効化 |
| `WOTOHA_AUTOMIX_CROSSFADE_SECONDS` | `8.0` | 希望する最大クロスフェード時間 |
| `WOTOHA_AUTOMIX_MAX_TEMPO_ADJUSTMENT` | `0.06` | Beat Matchで許可する最大テンポ調整率 |
| `WOTOHA_AUTOMIX_MIN_BEAT_CONFIDENCE` | `0.70` | Beat Matchに必要な最低ビート信頼度 |
| `WOTOHA_LOUDNESS_NORMALIZATION_ENABLED` | `true` | 曲ごとのラウドネス正規化を有効化 |
| `WOTOHA_LOUDNESS_TARGET_LUFS` | `-16.0` | Integrated Loudnessの目標値 |
| `WOTOHA_LOUDNESS_MAX_BOOST_DB` | `6.0` | 正規化で許可する最大ブースト量 |
| `WOTOHA_LOUDNESS_TRUE_PEAK_CEILING_DBTP` | `-2.0` | True Peakの上限 |
| `WOTOHA_MAX_QUEUE_LEN` | `512` | Discordサーバーごとの最大キュー長 |

すべての設定項目は [`deploy/wotoha.env.example`](deploy/wotoha.env.example) を参照してください。

## ソースからビルド

使用するRust toolchainは [`rust-toolchain.toml`](rust-toolchain.toml) で固定されています。

```bash
git clone https://github.com/NcaMoq/wotoha-rs.git
cd wotoha-rs
cargo build --release --bin wotoha-app
```

ローカル開発では、少なくとも `DISCORD_TOKEN` を設定した `.env` を作成してから実行します。

```bash
cargo run -p wotoha-app
```

品質チェックは次のコマンドで実行できます。

```bash
cargo fmt --all -- --check
cargo test --workspace --locked
cargo clippy --workspace --all-targets --no-deps -- -D warnings
```

## Linuxでの運用資料

ここからは、独自のWotohaインスタンスを管理する運用者向けの内容です。通常は[WotohaをDiscordへ追加](https://discord.com/oauth2/authorize?client_id=1238488423208063107)するだけで利用できます。

公式リリースは **x86_64 Linux** を対象としています。持ち運び向けのアプリ本体のみのアーカイブ（`wotoha-linux-x86_64-musl.tar.gz`）と、既存のアップデーターと互換性のあるアーカイブ（`wotoha-ubuntu-x86_64-musl.tar.gz`）を用意します。後者にはsystemd unitと導入スクリプトが含まれますが、どちらのアーカイブにもyt-dlpとDenoは再配布しません。導入時にインストーラーが公式GitHub Releaseから固定バージョンを直接取得し、yt-dlpの署名鍵フィンガープリントと署名付きチェックサム、Denoの固定SHA-256、バージョンと抽出canaryを検証してから原子的に導入します。Wotoha本体は静的リンクされています。インストーラーと自動更新は、systemdと標準的なGNUツールを利用できるLinux環境を想定しています。上流のDenoを利用するため、glibcベースのディストリビューションを推奨します。

使用するディストリビューションのパッケージマネージャーで、`ca-certificates`、`coreutils`、`curl`、GnuPG、`jq`、`tar`、`unzip`、`util-linux`を導入します。Debian・Ubuntuの場合は次のとおりです。

```bash
sudo apt update
sudo apt install -y ca-certificates coreutils curl gnupg jq tar unzip util-linux
```

公式Releaseを展開前に検証するには、`gh attestation verify` を利用できる最新の
[GitHub CLI](https://github.com/cli/cli#installation) が必要です。この検証では変動する
`latest` のダウンロードURLを使わず、公開済みの特定タグを選びます。以下の `vX.Y.Z` を
置き換え、すべてのコマンドが成功するまでアーカイブを展開・実行しないでください。

下記のアーカイブ、`.sha256`、`.manifest.json`、`.intoto.jsonl` がすべてAssetsに
掲載されているReleaseだけを使用してください。この一式がない過去のReleaseは、この
検証手順には対応していません。

```bash
(
set -euo pipefail
REPO=NcaMoq/wotoha-rs
TAG=vX.Y.Z
ASSET=wotoha-ubuntu-x86_64-musl.tar.gz
MANIFEST=wotoha-ubuntu-x86_64-musl.manifest.json
BUNDLE=wotoha-ubuntu-x86_64-musl.intoto.jsonl
BASE="https://github.com/$REPO/releases/download/$TAG"

for FILE in "$ASSET" "$ASSET.sha256" "$MANIFEST" "$BUNDLE"; do
  curl --fail --location --remote-name "$BASE/$FILE"
done

gh attestation verify --help | grep -q -- '--deny-self-hosted-runners'
gh attestation verify "$MANIFEST" \
  --bundle "$BUNDLE" --repo "$REPO" \
  --signer-workflow "$REPO/.github/workflows/release.yml" \
  --source-ref "refs/tags/$TAG" --deny-self-hosted-runners
COMMIT="$(jq -er '.commit | select(type == "string" and test("^[0-9a-f]{40}$"))' "$MANIFEST")"
DIGEST="$(sha256sum "$ASSET" | awk '{print $1}')"
SIZE="$(stat --format=%s "$ASSET")"
jq --exit-status --arg tag "$TAG" --arg commit "$COMMIT" \
  --arg asset "$ASSET" --arg digest "$DIGEST" --argjson size "$SIZE" '
  .schema_version == 1 and .tag == $tag and .commit == $commit
  and .asset == $asset and .sha256 == $digest and .size == $size
' "$MANIFEST" >/dev/null
for SUBJECT in "$ASSET" "$MANIFEST"; do
  gh attestation verify "$SUBJECT" \
    --bundle "$BUNDLE" --repo "$REPO" \
    --signer-workflow "$REPO/.github/workflows/release.yml" \
    --source-ref "refs/tags/$TAG" --source-digest "$COMMIT" \
    --deny-self-hosted-runners
done
sha256sum --check --strict "$ASSET.sha256"
)
```

プロベナンス、manifest、チェックサムの検証がすべて成功した後にだけ、アーカイブを導入します。

```bash
tar -xzf wotoha-ubuntu-x86_64-musl.tar.gz
cd wotoha-ubuntu-x86_64-musl
sudo bash ./install-ubuntu.sh
sudoedit /etc/wotoha/wotoha.env
sudo systemctl restart wotoha.service
```

現在の詳しい運用手順では、具体例としてUbuntuのコマンドを使用しています。配布物の検証、自動更新、ロールバック、Windowsからの手動パッケージ作成については、[Linux Server導入手順](docs/ubuntu-deploy.md)を参照してください。

## ドキュメント

- [最新のGitHub Release](https://github.com/NcaMoq/wotoha-rs/releases/latest)
- [YouTube抽出とyt-dlpの管理](docs/youtube-extraction.md)
- [Linux導入と自動更新（Ubuntuのコマンド例）](docs/ubuntu-deploy.md)
- [単一Linuxホスト構成と運用（Ubuntuリファレンス）](docs/single-host-ubuntu.md)

## ライセンス

Wotoha RSのプロジェクトコードは [MIT License](LICENSE) のもとで公開しています。
リリースアーカイブには、それぞれのライセンスが適用される第三者コンポーネントを含む場合があります。
配布およびソース入手先については [第三者通知](THIRD_PARTY_NOTICES.md) を参照してください。

## コントリビューションとサポート

不具合報告、再生互換性の報告、機能提案、Pull Requestは[GitHub Issues](https://github.com/NcaMoq/wotoha-rs/issues)から歓迎します。メディア再生の問題を報告する場合は、サービス名、URLの種類、Wotohaのバージョン、機密情報を除いた関連ログを添えてください。

Wotoha RSは、元のDiscord音楽Bot [Wotoha](https://github.com/NcaMoq/wotoha)をベースにRustで再設計・再実装したプロジェクトです。
