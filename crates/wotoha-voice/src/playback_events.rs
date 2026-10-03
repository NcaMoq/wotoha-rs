use std::collections::VecDeque;

use wotoha_contracts::PlaybackRuntimeEvent;

pub(crate) const MAX_CRITICAL_EVENT_OVERFLOW: usize = 64;

pub(crate) fn is_critical_runtime_event(event: &PlaybackRuntimeEvent) -> bool {
    matches!(
        event,
        PlaybackRuntimeEvent::TrackEnded { .. }
            | PlaybackRuntimeEvent::TransitionDue { .. }
            | PlaybackRuntimeEvent::TransitionStarted { .. }
            | PlaybackRuntimeEvent::TransitionArmFailed { .. }
            | PlaybackRuntimeEvent::TrackErrored { .. }
            | PlaybackRuntimeEvent::VoiceDisconnected { .. }
    )
}

pub(crate) fn queue_critical_overflow(
    overflow: &mut VecDeque<PlaybackRuntimeEvent>,
    event: PlaybackRuntimeEvent,
) -> bool {
    if overflow
        .iter()
        .any(|queued| same_critical_identity(queued, &event))
    {
        return true;
    }
    if overflow.len() >= MAX_CRITICAL_EVENT_OVERFLOW {
        return false;
    }
    overflow.push_back(event);
    true
}

fn same_critical_identity(left: &PlaybackRuntimeEvent, right: &PlaybackRuntimeEvent) -> bool {
    match (left, right) {
        (
            PlaybackRuntimeEvent::TrackEnded {
                guild_id: left_guild,
                session_id: left_session,
                playback_id: left_playback,
                ..
            },
            PlaybackRuntimeEvent::TrackEnded {
                guild_id: right_guild,
                session_id: right_session,
                playback_id: right_playback,
                ..
            },
        )
        | (
            PlaybackRuntimeEvent::TrackErrored {
                guild_id: left_guild,
                session_id: left_session,
                playback_id: left_playback,
                ..
            },
            PlaybackRuntimeEvent::TrackErrored {
                guild_id: right_guild,
                session_id: right_session,
                playback_id: right_playback,
                ..
            },
        ) => {
            left_guild == right_guild
                && left_session == right_session
                && left_playback == right_playback
        }
        (
            PlaybackRuntimeEvent::TransitionDue {
                guild_id: left_guild,
                session_id: left_session,
                playback_id: left_playback,
            },
            PlaybackRuntimeEvent::TransitionDue {
                guild_id: right_guild,
                session_id: right_session,
                playback_id: right_playback,
            },
        ) => {
            left_guild == right_guild
                && left_session == right_session
                && left_playback == right_playback
        }
        (
            PlaybackRuntimeEvent::TransitionStarted {
                guild_id: left_guild,
                session_id: left_session,
                outgoing_playback_id: left_outgoing,
                incoming_playback_id: left_incoming,
                generation: left_generation,
                ..
            }
            | PlaybackRuntimeEvent::TransitionArmFailed {
                guild_id: left_guild,
                session_id: left_session,
                outgoing_playback_id: left_outgoing,
                incoming_playback_id: left_incoming,
                generation: left_generation,
                ..
            },
            PlaybackRuntimeEvent::TransitionStarted {
                guild_id: right_guild,
                session_id: right_session,
                outgoing_playback_id: right_outgoing,
                incoming_playback_id: right_incoming,
                generation: right_generation,
                ..
            }
            | PlaybackRuntimeEvent::TransitionArmFailed {
                guild_id: right_guild,
                session_id: right_session,
                outgoing_playback_id: right_outgoing,
                incoming_playback_id: right_incoming,
                generation: right_generation,
                ..
            },
        ) => {
            left_guild == right_guild
                && left_session == right_session
                && left_outgoing == right_outgoing
                && left_incoming == right_incoming
                && left_generation == right_generation
        }
        (
            PlaybackRuntimeEvent::VoiceDisconnected {
                guild_id: left_guild,
                ..
            },
            PlaybackRuntimeEvent::VoiceDisconnected {
                guild_id: right_guild,
                ..
            },
        ) => left_guild == right_guild,
        _ => false,
    }
}
