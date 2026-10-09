# Wotoha RS — Rust製Discord向けAutomatic DJ

日本語 · [English](README.md)

**Wotoha RS**は、Rustで作られたオープンソースの**Discord向けAutomatic DJ**です。固定のクロスフェードを毎回適用するのではなく、BPM/テンポとビート、曲構造、音圧、ボーカル、調性などの音響情報を使って曲間のつなぎ方を計画します。

<p align="center">
  <a href="https://discord.com/oauth2/authorize?client_id=1238488423208063107"><img src="https://img.shields.io/badge/Wotoha%E3%82%92Discord%E3%81%AB%E8%BF%BD%E5%8A%A0-5865F2?style=for-the-badge&logo=discord&logoColor=white" alt="WotohaをDiscordに追加"></a>
</p>

[![最新リリース](https://img.shields.io/github/v/release/NcaMoq/wotoha-rs?sort=semver&label=release)](https://github.com/NcaMoq/wotoha-rs/releases/latest)
[![CI](https://github.com/NcaMoq/wotoha-rs/actions/workflows/ci.yml/badge.svg)](https://github.com/NcaMoq/wotoha-rs/actions/workflows/ci.yml)
[![Rust](https://img.shields.io/badge/Rust-2024-000000?logo=rust)](https://www.rust-lang.org/)
[![License](https://img.shields.io/github/license/NcaMoq/wotoha-rs)](LICENSE)

条件が合えばビートを合わせる **BeatMatched**、一般的な **Crossfade**、重ねない **Gapless** を選び、音響的に無理のある重ね合わせは安全な方式へフォールバックします。対応サービスからの再生をすぐに試せるほか、Dockerによるセルフホストにも対応しています。

> Wotoha RSが役に立ったら、[GitHubでStarを付けて](https://github.com/NcaMoq/wotoha-rs)もらえると嬉しいです。

[最新のGitHub Release](https://github.com/NcaMoq/wotoha-rs/releases/latest)を安定版の配布経路として使用してください。`main`には未リリースの開発・研究作業が含まれる場合があります。

## WotohaをDiscordに追加

1. [Wotohaの招待ページを開きます](https://discord.com/oauth2/authorize?client_id=1238488423208063107)。
2. 追加するDiscordサーバーを選び、インストールを承認します。
3. ボイスチャンネルに参加し、次を実行します。

```text
/play url:https://example.com/music
```

## Wotoha RSの特長

- **解析駆動のAutoMix** — テンポ、ビートの位置、曲構造、キュー、音圧、ボーカル、利用可能な場合は調性の相性を使って候補を評価します。
- **保守的な曲間処理** — 条件を満たすときだけBeatMatchedを使い、それ以外はCrossfadeまたはGaplessへ切り替えます。
- **音量とピークを管理** — 既定では `-16 LUFS` を目標にし、ブーストより減衰を優先しながら、正規化段階で `-2 dBTP` のTrue Peak上限を設けます。
- **Rustの音声処理スタック** — Tokio、Serenity、Songbird、Symphoniaを用いたCargo workspaceです。
- **セルフホスト可能** — 本番コンテナはLinux/amd64、非root、read-only root filesystem、capability削減、オフラインself-checkを前提にしています。

## AutoMixの仕組み

```text
再生中の曲                 次の曲
      │                       │
      └──── デコード + 解析 ───┘
           ビート / テンポ / 位相
        構造 / キュー / 音圧
        ボーカル / 調性 / ラウドネス
                     │
               有界なプランナー
               ┌──────┼──────┐
               │      │      │
          BeatMatched Crossfade Gapless
               └──────┼──────┘
             品質ガード + Peak Guard
                     │
                   再生
```

既定の本番authorityは互換性を重視したLegacy plannerです。AutoMix V2は`shadow`で観測するか、`WOTOHA_AUTOMIX_PLANNER_MODE=v2`を明示して制御された評価に使えます。解析ラボの研究結果が暗黙に本番へ切り替わることはありません。実装境界と各モードは[docs/automix.md](docs/automix.md)にまとめています。

## 対応するサービス

| サービス | 対応内容 |
| --- | --- |
| YouTube | 管理されたyt-dlpによる動画・音楽URL |
| SoundCloud | 公開トラック |
| Bandcamp | 公開トラックページ |
| ニコニコ動画 | 公開動画URL |
| Vimeo | 公開動画 |
| Twitch | ライブ配信とVOD |
| X / Twitter | 再生可能なメディアを含む投稿 |

各サービスの公開仕様が変わると、対応状況も変わる可能性があります。

## セルフホスト

本番運用はLinux/amd64上のDockerを使用します。[Dockerデプロイガイド](docs/docker-deploy.md)、[`compose.yaml`](compose.yaml)、[`runtime.env.example`](runtime.env.example)から始めてください。アップグレード、ロールバック、データ移行、セキュリティの詳細はデプロイ文書に分離しています。

ソースからの開発・ビルドは[docs/development.md](docs/development.md)、旧環境からの移行は[docs/ubuntu-deploy.md](docs/ubuntu-deploy.md)を参照してください。

## 音響解析の研究

リポジトリには、ビートやテンポの変更を管理された証拠で検証するclean-room analysis labがあります。研究用コマンドや生成物、外部のreference packetは再生authorityとは分離されています。[docs/analysis-lab.md](docs/analysis-lab.md)を参照してください。

## デモ

現時点では、リポジトリに録画済みのデモ素材はありません。[docs/demo-capture.md](docs/demo-capture.md)に、解析入力と選択された曲間処理を正確に見せるための短い撮影案をまとめています。

## ドキュメントとコントリビューション

- [AutoMixアーキテクチャ](docs/automix.md)
- [開発環境](docs/development.md)
- [Dockerデプロイ](docs/docker-deploy.md)
- [YouTube/プロバイダの取得メモ](docs/youtube-extraction.md)
- [Security](SECURITY.md) · [Privacy](PRIVACY.md)
- [コントリビューション](CONTRIBUTING.md)

IssueやPull Requestを歓迎します。認証情報、非公開メディア、外部観測のraw data、生成レポート、ビルド成果物はGit管理外に置いてください。

## ライセンス

Wotoha RSは[MIT License](LICENSE)で公開しています。第三者ライセンスは[THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md)にまとめています。
