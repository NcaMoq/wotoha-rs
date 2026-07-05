# Wotoha RS

**Wotoha RS** は、Discord音楽BOT **[Wotoha](https://github.com/NcaMoq/wotoha)** をベースに、Codexを使用してRustで再設計・再実装したものです。

DiscordのスラッシュコマンドからURLを指定することで、ボイスチャンネル内で音楽を再生できます。

## Commands

### `/play <url>`

指定したURLを再生キューに追加します。

再生開始後は、メッセージ上のボタンから以下の操作ができます。

* **Skip**: 現在再生中の曲をスキップします
* **Loop**: 現在の曲をループ再生します
* **Shuffle**: キュー内の曲順をシャッフルします
* **AutoMix**: 曲と曲をスムーズにつなぎます
* **List**: 現在の再生キューを表示します