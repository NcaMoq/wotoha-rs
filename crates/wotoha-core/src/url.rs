use std::net::IpAddr;

use url::Url;

#[derive(Clone, Copy)]
struct HostPolicy {
    exact: &'static [&'static str],
    suffixes: &'static [&'static str],
}

#[derive(Clone, Copy)]
struct ProviderUrlPolicy {
    provider_id: &'static str,
    track: HostPolicy,
    playback: HostPolicy,
}

const YOUTUBE_TRACK_POLICY: HostPolicy = HostPolicy {
    exact: &[
        "youtube.com",
        "www.youtube.com",
        "m.youtube.com",
        "music.youtube.com",
        "youtu.be",
    ],
    suffixes: &[],
};

const YOUTUBE_PLAYBACK_POLICY: HostPolicy = HostPolicy {
    exact: &[
        "googlevideo.com",
        "manifest.googlevideo.com",
        "youtube.com",
        "www.youtube.com",
        "music.youtube.com",
    ],
    suffixes: &[".googlevideo.com", ".youtube.com"],
};

const SOUNDCLOUD_TRACK_POLICY: HostPolicy = HostPolicy {
    exact: &[
        "soundcloud.com",
        "www.soundcloud.com",
        "m.soundcloud.com",
        "on.soundcloud.com",
    ],
    suffixes: &[],
};

const SOUNDCLOUD_PLAYBACK_POLICY: HostPolicy = HostPolicy {
    exact: &["sndcdn.com"],
    suffixes: &[".sndcdn.com"],
};

const X_TRACK_POLICY: HostPolicy = HostPolicy {
    exact: &[
        "twitter.com",
        "www.twitter.com",
        "mobile.twitter.com",
        "x.com",
        "www.x.com",
        "mobile.x.com",
    ],
    suffixes: &[],
};

const X_PLAYBACK_POLICY: HostPolicy = HostPolicy {
    exact: &["video.twimg.com"],
    suffixes: &[".video.twimg.com"],
};

const NICONICO_TRACK_POLICY: HostPolicy = HostPolicy {
    exact: &["nicovideo.jp", "www.nicovideo.jp", "nico.ms"],
    suffixes: &[],
};

const NICONICO_PLAYBACK_POLICY: HostPolicy = HostPolicy {
    exact: &[
        "nicovideo.jp",
        "www.nicovideo.jp",
        "delivery.domand.nicovideo.jp",
        "asset.domand.nicovideo.jp",
        "domand.nicovideo.jp",
        "dmc.nico",
        "api.dmc.nico",
    ],
    suffixes: &[
        ".nicovideo.jp",
        ".domand.nicovideo.jp",
        ".dmc.nico",
        ".nimg.jp",
    ],
};

const BANDCAMP_TRACK_POLICY: HostPolicy = HostPolicy {
    exact: &["bandcamp.com"],
    suffixes: &[".bandcamp.com"],
};

const BANDCAMP_PLAYBACK_POLICY: HostPolicy = HostPolicy {
    exact: &["bcbits.com"],
    suffixes: &[".bcbits.com"],
};

const VIMEO_TRACK_POLICY: HostPolicy = HostPolicy {
    exact: &["vimeo.com", "www.vimeo.com", "player.vimeo.com"],
    suffixes: &[],
};

const VIMEO_PLAYBACK_POLICY: HostPolicy = HostPolicy {
    exact: &[
        "player.vimeo.com",
        "vod-progressive.akamaized.net",
        "vod-adaptive.akamaized.net",
    ],
    suffixes: &[".vimeocdn.com"],
};

const TWITCH_TRACK_POLICY: HostPolicy = HostPolicy {
    exact: &["twitch.tv", "www.twitch.tv", "m.twitch.tv"],
    suffixes: &[],
};

const TWITCH_PLAYBACK_POLICY: HostPolicy = HostPolicy {
    exact: &["usher.ttvnw.net"],
    suffixes: &[".ttvnw.net"],
};

const PROVIDER_URL_POLICIES: &[ProviderUrlPolicy] = &[
    ProviderUrlPolicy {
        provider_id: "youtube",
        track: YOUTUBE_TRACK_POLICY,
        playback: YOUTUBE_PLAYBACK_POLICY,
    },
    ProviderUrlPolicy {
        provider_id: "soundcloud",
        track: SOUNDCLOUD_TRACK_POLICY,
        playback: SOUNDCLOUD_PLAYBACK_POLICY,
    },
    ProviderUrlPolicy {
        provider_id: "x",
        track: X_TRACK_POLICY,
        playback: X_PLAYBACK_POLICY,
    },
    ProviderUrlPolicy {
        provider_id: "niconico",
        track: NICONICO_TRACK_POLICY,
        playback: NICONICO_PLAYBACK_POLICY,
    },
    ProviderUrlPolicy {
        provider_id: "bandcamp",
        track: BANDCAMP_TRACK_POLICY,
        playback: BANDCAMP_PLAYBACK_POLICY,
    },
    ProviderUrlPolicy {
        provider_id: "vimeo",
        track: VIMEO_TRACK_POLICY,
        playback: VIMEO_PLAYBACK_POLICY,
    },
    ProviderUrlPolicy {
        provider_id: "twitch",
        track: TWITCH_TRACK_POLICY,
        playback: TWITCH_PLAYBACK_POLICY,
    },
];

pub fn is_allowed_track_url(raw_url: &str) -> bool {
    is_allowed_by_any(raw_url, |policy| policy.track)
}

pub fn is_allowed_prepared_url(provider_id: &str, raw_url: &str) -> bool {
    let Some(policy) = provider_policy(provider_id) else {
        return false;
    };

    is_allowed_url(raw_url, policy.playback)
}

pub fn is_allowed_runtime_redirect_url(raw_url: &str) -> bool {
    is_allowed_by_any(raw_url, |policy| policy.playback)
}

pub fn same_url_host(left: &str, right: &str) -> bool {
    let Ok(left) = Url::parse(left) else {
        return false;
    };
    let Ok(right) = Url::parse(right) else {
        return false;
    };

    match (normalize_host(&left), normalize_host(&right)) {
        (Some(left_host), Some(right_host)) => left_host == right_host,
        _ => false,
    }
}

pub fn summarize_url_for_logs(raw_url: &str) -> String {
    let Ok(url) = Url::parse(raw_url) else {
        return "<invalid-url>".to_owned();
    };

    let Some(host) = url.host_str() else {
        return "<invalid-url>".to_owned();
    };

    let redacted_path = if url.path().is_empty() || url.path() == "/" {
        "/"
    } else {
        "/[redacted]"
    };

    format!(
        "{}://{}{}",
        url.scheme(),
        host.to_ascii_lowercase(),
        redacted_path
    )
}

fn provider_policy(provider_id: &str) -> Option<ProviderUrlPolicy> {
    PROVIDER_URL_POLICIES
        .iter()
        .copied()
        .find(|policy| policy.provider_id == provider_id)
}

fn is_allowed_by_any<F>(raw_url: &str, policy_of: F) -> bool
where
    F: Fn(ProviderUrlPolicy) -> HostPolicy,
{
    let Ok(url) = Url::parse(raw_url) else {
        return false;
    };
    if url.scheme() != "https" {
        return false;
    }
    let Some(host) = normalize_host(&url) else {
        return false;
    };

    PROVIDER_URL_POLICIES
        .iter()
        .copied()
        .any(|policy| policy_matches_host(policy_of(policy), &host))
}

fn is_allowed_url(raw_url: &str, policy: HostPolicy) -> bool {
    let Ok(url) = Url::parse(raw_url) else {
        return false;
    };
    if url.scheme() != "https" {
        return false;
    }

    let Some(host) = normalize_host(&url) else {
        return false;
    };

    policy_matches_host(policy, &host)
}

fn policy_matches_host(policy: HostPolicy, host: &str) -> bool {
    policy.exact.contains(&host) || policy.suffixes.iter().any(|suffix| host.ends_with(suffix))
}

fn normalize_host(url: &Url) -> Option<String> {
    let host = url.host_str()?.to_ascii_lowercase();
    if host == "localhost" {
        return None;
    }
    if host.parse::<IpAddr>().is_ok() {
        return None;
    }
    Some(host)
}

#[cfg(test)]
mod tests {
    use super::{
        is_allowed_prepared_url, is_allowed_runtime_redirect_url, is_allowed_track_url,
        same_url_host, summarize_url_for_logs,
    };

    #[test]
    fn allows_expected_track_inputs() {
        assert!(is_allowed_track_url(
            "https://www.youtube.com/watch?v=dQw4w9WgXcQ"
        ));
        assert!(is_allowed_track_url(
            "https://cloudkicker.bandcamp.com/track/94-days"
        ));
        assert!(!is_allowed_track_url(
            "http://www.youtube.com/watch?v=dQw4w9WgXcQ"
        ));
        assert!(!is_allowed_track_url("https://127.0.0.1/track"));
    }

    #[test]
    fn allows_only_provider_specific_playback_hosts() {
        assert!(is_allowed_prepared_url(
            "youtube",
            "https://rr1---sn.example.googlevideo.com/videoplayback?id=123",
        ));
        assert!(is_allowed_prepared_url(
            "vimeo",
            "https://vod-progressive.akamaized.net/exp=1/video.mp4",
        ));
        assert!(!is_allowed_prepared_url(
            "youtube",
            "https://example.com/videoplayback",
        ));
    }

    #[test]
    fn runtime_redirect_allowlist_covers_known_media_hosts() {
        assert!(is_allowed_runtime_redirect_url(
            "https://cf-hls-media.sndcdn.com/media/playlist.m3u8",
        ));
        assert!(is_allowed_runtime_redirect_url(
            "https://video-weaver.tyo01.hls.ttvnw.net/v1/playlist.m3u8",
        ));
        assert!(!is_allowed_runtime_redirect_url(
            "https://malicious.example/playlist.m3u8",
        ));
    }

    #[test]
    fn log_summary_redacts_path_query_and_fragment() {
        let summary = summarize_url_for_logs("https://vimeo.com/76979871/secretshare?h=abc#frag");
        assert_eq!(summary, "https://vimeo.com/[redacted]");
    }

    #[test]
    fn compares_hosts_after_normalization() {
        assert!(same_url_host(
            "https://www.nicovideo.jp/watch/sm9",
            "https://www.nicovideo.jp/api/watch/v3/sm9",
        ));
        assert!(!same_url_host(
            "https://www.nicovideo.jp/watch/sm9",
            "https://asset.domand.nicovideo.jp/media/segment.ts",
        ));
    }
}
