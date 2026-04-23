use std::collections::VecDeque;

use rand::seq::SliceRandom;

use crate::model::TrackRequest;

#[derive(Clone, Debug, Default)]
pub struct QueuePreview {
    current: Option<TrackRequest>,
    upcoming: Vec<TrackRequest>,
    total_queued: usize,
}

impl QueuePreview {
    pub fn current(&self) -> Option<&TrackRequest> {
        self.current.as_ref()
    }

    pub fn upcoming(&self) -> &[TrackRequest] {
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
}

impl GuildPlayerState {
    pub fn enqueue(&mut self, track: TrackRequest) -> bool {
        if self.current.is_none() {
            self.current = Some(track);
            true
        } else {
            self.queue.push_back(track);
            false
        }
    }

    pub fn current(&self) -> Option<&TrackRequest> {
        self.current.as_ref()
    }

    pub fn current_cloned(&self) -> Option<TrackRequest> {
        self.current.clone()
    }

    pub fn queue(&self) -> &VecDeque<TrackRequest> {
        &self.queue
    }

    pub fn queue_len(&self) -> usize {
        self.queue.len()
    }

    pub fn queue_preview(&self, limit: usize) -> QueuePreview {
        QueuePreview {
            current: self.current.clone(),
            upcoming: self.queue.iter().take(limit).cloned().collect(),
            total_queued: self.queue.len(),
        }
    }

    pub fn is_looping(&self) -> bool {
        self.looping
    }

    pub fn toggle_loop(&mut self) -> bool {
        self.looping = !self.looping;
        self.looping
    }

    pub fn disable_loop(&mut self) -> bool {
        let previous = self.looping;
        self.looping = false;
        previous
    }

    pub fn prepare_next_track(&mut self) -> Option<TrackRequest> {
        let next = if self.looping {
            self.current.clone()
        } else {
            self.queue.pop_front()
        };

        self.current = next.clone();
        next
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
    }

    pub fn shuffle(&mut self) -> bool {
        if self.queue.is_empty() {
            return false;
        }

        let items = self.queue.make_contiguous();
        let mut rng = rand::rng();
        items.shuffle(&mut rng);
        true
    }
}
