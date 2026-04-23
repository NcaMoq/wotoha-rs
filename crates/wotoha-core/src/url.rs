use url::Url;

const ALLOWED_TRACK_HOSTS: &[&str] = &[
    "youtube.com",
    "www.youtube.com",
    "m.youtube.com",
    "music.youtube.com",
    "youtu.be",
    "soundcloud.com",
    "www.soundcloud.com",
    "m.soundcloud.com",
    "nicovideo.jp",
    "www.nicovideo.jp",
    "nico.ms",
    "bandcamp.com",
    "vimeo.com",
    "www.vimeo.com",
    "player.vimeo.com",
    "twitch.tv",
    "www.twitch.tv",
    "m.twitch.tv",
];

const ALLOWED_TRACK_HOST_SUFFIXES: &[&str] = &[".bandcamp.com"];

pub fn is_allowed_track_url(raw_url: &str) -> bool {
    let Ok(url) = Url::parse(raw_url) else {
        return false;
    };

    if url.scheme() != "https" {
        return false;
    }

    let Some(host) = url.host_str() else {
        return false;
    };

    let normalized_host = host.to_ascii_lowercase();
    ALLOWED_TRACK_HOSTS.contains(&normalized_host.as_str())
        || ALLOWED_TRACK_HOST_SUFFIXES
            .iter()
            .any(|suffix| normalized_host.ends_with(suffix))
}
