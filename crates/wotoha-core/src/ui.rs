use std::time::Duration;

pub const PLAY_COMMAND_NAME: &str = "play";
pub const PLAY_COMMAND_DESCRIPTION: &str = "音楽を再生";
pub const PLAY_COMMAND_URL_OPTION: &str = "url";

pub const BUTTON_SKIP: &str = "player_skip";
pub const BUTTON_LOOP: &str = "player_loop";
pub const BUTTON_SHUFFLE: &str = "player_shuffle";
pub const BUTTON_AUTOMIX: &str = "player_automix";
pub const BUTTON_QUEUE: &str = "player_queue";

pub const BUTTON_SKIP_LABEL: &str = "Skip";
pub const BUTTON_LOOP_LABEL: &str = "Loop";
pub const BUTTON_SHUFFLE_LABEL: &str = "Shuffle";
pub const BUTTON_AUTOMIX_LABEL: &str = "AutoMix";
pub const BUTTON_QUEUE_LABEL: &str = "List";

pub const SKIP_EMOJI_NAME: &str = "skip";
pub const LOOP_EMOJI_NAME: &str = "loop";
pub const SHUFFLE_EMOJI_NAME: &str = "shuffle";
pub const QUEUE_EMOJI_NAME: &str = "list";

pub const SKIP_EMOJI_ID: u64 = 1450137384359559332;
pub const LOOP_EMOJI_ID: u64 = 1450137411278475416;
pub const SHUFFLE_EMOJI_ID: u64 = 1450135594746511393;
pub const QUEUE_EMOJI_ID: u64 = 1450138747084738751;

pub const COLOR_INFO: u32 = 0x49B0E4;
pub const COLOR_ERROR: u32 = 0xE74C3C;

pub const LOOPING_NICKNAME: &str = "音葉 🔁";

pub const MSG_JOIN_VOICE_FIRST: &str = "ボイスチャンネルに参加してください。";
pub const MSG_ALLOWED_URL_ONLY: &str = "許可されている HTTPS の音源URLのみ再生できます。";
pub const MSG_NO_TRACK_PLAYING: &str = "再生中の曲がありません。";
pub const MSG_NOTHING_TO_SHUFFLE: &str = "シャッフルできる曲がありません。";
pub const MSG_SHUFFLED: &str = "プレイリストをシャッフルしました！";
pub const MSG_QUEUE_EMPTY: &str = "プレイリストは空です。";
pub const MSG_JOIN_ACTIVE_VOICE: &str = "再生中のボイスチャンネルに参加してください。";
pub const MSG_PLAYING_IN_ANOTHER_VOICE: &str =
    "別のボイスチャンネルで再生中です。同じ部屋から操作してください。";

pub fn format_duration(duration: Option<Duration>) -> String {
    let Some(duration) = duration else {
        return "--m --s".to_owned();
    };

    let minutes = duration.as_secs() / 60;
    let seconds = duration.as_secs() % 60;
    format!("{minutes:02}m {seconds:02}s")
}
