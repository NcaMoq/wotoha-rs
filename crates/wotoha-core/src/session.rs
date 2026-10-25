use std::collections::VecDeque;

use rand::Rng;

use crate::model::{TrackMetadata, TrackRequest, TrackRequestId};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TrackPreview {
    pub request_id: TrackRequestId,
    pub metadata: TrackMetadata,
}

impl From<&TrackRequest> for TrackPreview {
    fn from(request: &TrackRequest) -> Self {
        Self {
            request_id: request.request_id,
            metadata: request.metadata.clone(),
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct QueuePreview {
    current: Option<TrackPreview>,
    upcoming: Vec<TrackPreview>,
    total_queued: usize,
}

impl QueuePreview {
    pub fn current(&self) -> Option<&TrackPreview> {
        self.current.as_ref()
    }

    pub fn upcoming(&self) -> &[TrackPreview] {
        &self.upcoming
    }

    pub fn total_queued(&self) -> usize {
        self.total_queued
    }
}

#[derive(Clone, Debug, Default)]
pub struct GuildPlayerState {
    current: Option<TrackRequest>,
    queue: VecDeque<TrackRequest>,
    looping: bool,
    shuffle_enabled: bool,
    shuffle_next_index: Option<usize>,
}

impl GuildPlayerState {
    pub fn enqueue(&mut self, track: TrackRequest) -> bool {
        if self.current.is_none() {
            self.current = Some(track);
            true
        } else {
            self.queue.push_back(track);
            self.ensure_shuffle_choice();
            false
        }
    }

    pub fn current(&self) -> Option<&TrackRequest> {
        self.current.as_ref()
    }

    pub fn queue(&self) -> &VecDeque<TrackRequest> {
        &self.queue
    }

    pub fn upcoming_tracks(&self, limit: usize) -> Vec<&TrackRequest> {
        let mut upcoming = Vec::with_capacity(limit.min(self.queue.len()));
        let selected_index = self.selected_shuffle_index();
        if limit > 0
            && let Some(index) = selected_index
            && let Some(track) = self.queue.get(index)
        {
            upcoming.push(track);
        }

        for (index, track) in self.queue.iter().enumerate() {
            if Some(index) == selected_index {
                continue;
            }
            if upcoming.len() >= limit {
                break;
            }
            upcoming.push(track);
        }
        upcoming
    }

    pub fn queue_preview(&self, limit: usize) -> QueuePreview {
        QueuePreview {
            current: self.current.as_ref().map(TrackPreview::from),
            upcoming: self
                .upcoming_tracks(limit)
                .into_iter()
                .map(TrackPreview::from)
                .collect(),
            total_queued: self.queue.len(),
        }
    }

    pub fn toggle_loop(&mut self) -> bool {
        self.looping = !self.looping;
        if self.looping {
            self.shuffle_enabled = false;
            self.shuffle_next_index = None;
        }
        self.looping
    }

    pub fn disable_loop(&mut self) -> bool {
        let previous = self.looping;
        self.looping = false;
        previous
    }

    pub fn is_looping(&self) -> bool {
        self.looping
    }

    pub fn toggle_shuffle(&mut self) -> bool {
        self.shuffle_enabled = !self.shuffle_enabled;
        if self.shuffle_enabled {
            self.looping = false;
            self.ensure_shuffle_choice();
        } else {
            self.shuffle_next_index = None;
        }
        self.shuffle_enabled
    }

    pub fn set_shuffle_enabled(&mut self, enabled: bool) {
        self.shuffle_enabled = enabled;
        if enabled {
            self.looping = false;
            self.ensure_shuffle_choice();
        } else {
            self.shuffle_next_index = None;
        }
    }

    pub fn is_shuffling(&self) -> bool {
        self.shuffle_enabled
    }

    pub fn prepare_next_track(&mut self) -> Option<TrackRequest> {
        let next = if self.looping {
            self.current.clone()
        } else if self.shuffle_enabled {
            self.ensure_shuffle_choice();
            let next = self
                .selected_shuffle_index()
                .and_then(|index| self.queue.remove(index));
            self.shuffle_next_index = None;
            self.ensure_shuffle_choice();
            next
        } else {
            self.queue.pop_front()
        };

        self.current = next.clone();
        next
    }

    pub fn peek_next_track(&self) -> Option<&TrackRequest> {
        if self.looping {
            self.current.as_ref()
        } else if self.shuffle_enabled {
            self.selected_shuffle_index()
                .and_then(|index| self.queue.get(index))
        } else {
            self.queue.front()
        }
    }

    pub fn replace_current(&mut self, track: TrackRequest) {
        self.current = Some(track);
    }

    pub fn clear_current(&mut self) {
        self.current = None;
    }

    pub fn clear(&mut self) {
        self.current = None;
        self.queue.clear();
        self.looping = false;
        self.shuffle_enabled = false;
        self.shuffle_next_index = None;
    }

    fn selected_shuffle_index(&self) -> Option<usize> {
        if !self.shuffle_enabled || self.queue.is_empty() {
            return None;
        }
        Some(
            self.shuffle_next_index
                .filter(|index| *index < self.queue.len())
                .unwrap_or(0),
        )
    }

    fn ensure_shuffle_choice(&mut self) {
        if !self.shuffle_enabled || self.queue.is_empty() || self.shuffle_next_index.is_some() {
            return;
        }

        self.shuffle_next_index = Some(rand::rng().random_range(0..self.queue.len()));
    }
}

#[cfg(test)]
mod tests {
    use crate::model::{PreparedSource, TrackMetadata, TrackRequest};

    use super::GuildPlayerState;

    fn track(name: &str) -> TrackRequest {
        let url = format!("https://example.com/{name}");
        TrackRequest::new(
            name,
            url.as_str(),
            url.as_str(),
            url.as_str(),
            url.as_str(),
            PreparedSource::http(url.as_str(), Vec::new(), None, None),
            TrackMetadata::new(name, "artist", url.as_str(), None, None),
        )
    }

    #[test]
    fn shuffle_selects_a_stable_random_queue_item_without_reordering_the_queue() {
        let mut state = GuildPlayerState::default();
        state.enqueue(track("current"));
        state.enqueue(track("first"));
        state.enqueue(track("second"));
        state.enqueue(track("third"));

        assert!(state.toggle_shuffle());
        let selected = state.peek_next_track().unwrap().metadata.title.to_string();
        assert!(matches!(selected.as_str(), "first" | "second" | "third"));
        assert_eq!(
            state.peek_next_track().unwrap().metadata.title.as_ref(),
            selected.as_str()
        );
        assert_eq!(
            state.queue_preview(3).upcoming()[0].metadata.title.as_ref(),
            selected.as_str()
        );
        assert_eq!(
            state
                .queue()
                .iter()
                .map(|item| item.metadata.title.as_ref())
                .collect::<Vec<_>>(),
            ["first", "second", "third"]
        );

        let played = state.prepare_next_track().unwrap();
        assert_eq!(played.metadata.title.as_ref(), selected.as_str());
        let remaining = state
            .queue()
            .iter()
            .map(|item| item.metadata.title.as_ref())
            .collect::<Vec<_>>();
        let original_remaining = ["first", "second", "third"]
            .into_iter()
            .filter(|name| *name != selected)
            .collect::<Vec<_>>();
        assert_eq!(remaining, original_remaining);
    }

    #[test]
    fn enabling_loop_disables_shuffle_and_restores_fifo_order() {
        let mut state = GuildPlayerState::default();
        state.enqueue(track("current"));
        state.enqueue(track("first"));
        state.enqueue(track("second"));
        assert!(state.toggle_shuffle());
        assert!(state.toggle_loop());

        assert!(state.is_looping());
        assert!(!state.is_shuffling());
        assert_eq!(
            state.prepare_next_track().unwrap().metadata.title.as_ref(),
            "current"
        );
        assert!(state.disable_loop());
        assert_eq!(
            state.prepare_next_track().unwrap().metadata.title.as_ref(),
            "first"
        );
    }

    #[test]
    fn enabling_shuffle_disables_loop_and_selects_a_queued_track() {
        let mut state = GuildPlayerState::default();
        state.enqueue(track("current"));
        state.enqueue(track("first"));
        state.enqueue(track("second"));
        assert!(state.toggle_loop());

        assert!(state.toggle_shuffle());

        assert!(!state.is_looping());
        assert!(state.is_shuffling());
        assert!(matches!(
            state.peek_next_track().unwrap().metadata.title.as_ref(),
            "first" | "second"
        ));
    }

    #[test]
    fn disabling_shuffle_restores_fifo_selection() {
        let mut state = GuildPlayerState::default();
        state.enqueue(track("current"));
        state.enqueue(track("first"));
        state.enqueue(track("second"));
        assert!(state.toggle_shuffle());
        assert!(!state.toggle_shuffle());
        assert_eq!(
            state.peek_next_track().unwrap().metadata.title.as_ref(),
            "first"
        );
    }

    #[test]
    fn clear_resets_loop_and_shuffle_modes() {
        let mut state = GuildPlayerState::default();
        state.enqueue(track("current"));
        state.enqueue(track("queued"));
        state.toggle_loop();
        state.clear();

        assert!(!state.is_looping());
        assert!(!state.is_shuffling());
        assert!(state.current().is_none());
        assert!(state.queue().is_empty());

        state.enqueue(track("next-current"));
        state.enqueue(track("next-queued"));
        state.toggle_shuffle();
        state.clear();

        assert!(!state.is_looping());
        assert!(!state.is_shuffling());
    }
}
