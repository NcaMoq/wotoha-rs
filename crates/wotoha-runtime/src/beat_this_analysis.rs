//! Serialized, offline Beat This! model access.
//!
//! The model files are compiled into the runtime binary as release assets. No
//! URL or model download path exists in this module. A single `BeatThis` instance
//! is protected by a mutex because
//! rten model execution is mutable; this also keeps lookahead analyses from
//! multiplying the model's working set.

use std::sync::{
    Mutex, OnceLock,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};

use beat_this::BeatThis;
use sha2::{Digest, Sha256};
use wotoha_core::beat_analysis::NeuralBeatObservations;

use crate::embedded_rten::EmbeddedRtenRuntime;

const SMALL_MODEL_SHA256: &str = "a5f8d39d989f31859454ba27afe61c5317ca95e4d9373e6853e5361b8937172f";
const MEL_MODEL_SHA256: &str = "fdd59e65c515331308e4c8841edf99972deca646bdf6197744c2a5b7755e3de9";
const BEAT_THIS_VERSION: &str = "1.0.0";
pub const BEAT_THIS_VCS: &str = "089b509247e6fdcec666511c0dcf0d5f39c21e73";
const SMALL_MODEL_ORIGIN: &str = "https://github.com/danigb/beat-this-rs/blob/089b509247e6fdcec666511c0dcf0d5f39c21e73/models/beat_this_small.onnx";
const MEL_MODEL_ORIGIN: &str = "https://github.com/danigb/beat-this-rs/blob/089b509247e6fdcec666511c0dcf0d5f39c21e73/models/mel_spectrogram.onnx";
const MODEL_LICENSE: &str = "MIT (Beat This! / beat-this-rs)";

static SMALL_MODEL: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/models/beat_this_small.onnx"
));
static MEL_MODEL: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/models/mel_spectrogram.onnx"
));

/// Release-notice metadata for the two offline model assets.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BeatModelAssetMetadata {
    pub file_name: &'static str,
    pub sha256: &'static str,
    pub origin: &'static str,
    pub license: &'static str,
}

pub const BEAT_MODEL_ASSETS: [BeatModelAssetMetadata; 2] = [
    BeatModelAssetMetadata {
        file_name: "beat_this_small.onnx",
        sha256: SMALL_MODEL_SHA256,
        origin: SMALL_MODEL_ORIGIN,
        license: MODEL_LICENSE,
    },
    BeatModelAssetMetadata {
        file_name: "mel_spectrogram.onnx",
        sha256: MEL_MODEL_SHA256,
        origin: MEL_MODEL_ORIGIN,
        license: MODEL_LICENSE,
    },
];

type Tracker = Box<dyn FnMut(&[f32], u32) -> Option<NeuralBeatObservations> + Send>;

// `beat-this` intentionally keeps its backend model type private. Store the
// concrete tracker behind a closure so this crate still owns one initialized
// model without depending on an implementation-private type. The mutex is
// initialized independently from the model: a failed load must remain
// retryable, rather than becoming a process-lifetime cached error.
struct TrackerState {
    tracker: Option<Tracker>,
}

static TRACKER: OnceLock<Mutex<TrackerState>> = OnceLock::new();
static INITIALIZATION_COUNT: AtomicUsize = AtomicUsize::new(0);
static ACTIVE_INFERENCE: AtomicUsize = AtomicUsize::new(0);
static MAX_ACTIVE_INFERENCE: AtomicUsize = AtomicUsize::new(0);

struct InferenceGuard;

impl InferenceGuard {
    fn acquire() -> Self {
        let active = ACTIVE_INFERENCE.fetch_add(1, Ordering::AcqRel) + 1;
        MAX_ACTIVE_INFERENCE.fetch_max(active, Ordering::AcqRel);
        Self
    }
}

impl Drop for InferenceGuard {
    fn drop(&mut self) {
        ACTIVE_INFERENCE.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Run the bundled model once and expose its raw activation streams.
#[cfg(test)]
pub(crate) fn analyze(samples: &[f32], sample_rate: u32) -> Option<NeuralBeatObservations> {
    analyze_with_cancel(samples, sample_rate, &AtomicBool::new(false))
}

pub(crate) fn analyze_with_cancel(
    samples: &[f32],
    sample_rate: u32,
    cancelled: &AtomicBool,
) -> Option<NeuralBeatObservations> {
    if samples.is_empty() || sample_rate == 0 || samples.iter().any(|sample| !sample.is_finite()) {
        return None;
    }
    if cancelled.load(Ordering::Acquire) {
        return None;
    }
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mutex = TRACKER.get_or_init(|| Mutex::new(TrackerState { tracker: None }));
        // A prior panic poisons the mutex, but its state is still safe to
        // inspect and repair. Never turn poisoning into a permanent fallback.
        let mut state = lock_tracker(mutex);
        if state.tracker.is_none() {
            INITIALIZATION_COUNT.fetch_add(1, Ordering::Relaxed);
        }
        if !ensure_tracker(&mut state, build_tracker) {
            return None;
        }
        if cancelled.load(Ordering::Acquire) {
            return None;
        }
        let _inference = InferenceGuard::acquire();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            state
                .tracker
                .as_mut()
                .and_then(|tracker| tracker(samples, sample_rate))
        }));
        match result {
            Ok(result) if !cancelled.load(Ordering::Acquire) => result,
            Ok(_) => None,
            Err(_) => {
                // Do not reuse a tracker whose mutable backend panicked part
                // way through an inference. The next call retries a clean
                // engine while still using this same serialization gate.
                state.tracker = None;
                None
            }
        }
    }))
    .ok()
    .flatten()
}

fn lock_tracker(mutex: &Mutex<TrackerState>) -> std::sync::MutexGuard<'_, TrackerState> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn ensure_tracker(
    state: &mut TrackerState,
    mut builder: impl FnMut() -> Result<Tracker, String>,
) -> bool {
    if state.tracker.is_some() {
        return true;
    }
    let Ok(tracker) = builder() else {
        return false;
    };
    state.tracker = Some(tracker);
    true
}

fn build_tracker() -> Result<Tracker, String> {
    verify_assets()?;
    let runtime = EmbeddedRtenRuntime::new(MEL_MODEL, SMALL_MODEL);
    let mut tracker = BeatThis::new(
        &runtime,
        std::path::Path::new("mel_spectrogram.onnx"),
        std::path::Path::new("beat_this_small.onnx"),
    )
    .map_err(|error| {
        format!("Beat This! {BEAT_THIS_VERSION} model initialization failed: {error}")
    })?;
    let analyzer: Tracker = Box::new(move |samples, sample_rate| {
        let result = tracker.analyze_audio(samples, sample_rate).ok()?;
        if result.beat_logits.len() != result.downbeat_logits.len()
            || result.beat_logits.is_empty()
            || result
                .beat_logits
                .iter()
                .chain(result.downbeat_logits.iter())
                .any(|value| !value.is_finite())
        {
            return None;
        }
        NeuralBeatObservations::new(result.beat_logits, result.downbeat_logits)
    });
    Ok(analyzer)
}

pub fn model_assets() -> &'static [BeatModelAssetMetadata; 2] {
    &BEAT_MODEL_ASSETS
}

fn verify_assets() -> Result<(), String> {
    verify_hash("beat_this_small.onnx", SMALL_MODEL, SMALL_MODEL_SHA256)?;
    verify_hash("mel_spectrogram.onnx", MEL_MODEL, MEL_MODEL_SHA256)
}

fn verify_hash(name: &str, bytes: &[u8], expected: &str) -> Result<(), String> {
    let digest = Sha256::digest(bytes);
    let actual = digest
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    if actual == expected {
        Ok(())
    } else {
        Err(format!(
            "bundled {name} hash mismatch: expected {expected}, got {actual}"
        ))
    }
}

#[cfg(test)]
pub(crate) fn test_inference_counters() -> (usize, usize) {
    (
        INITIALIZATION_COUNT.load(Ordering::Acquire),
        MAX_ACTIVE_INFERENCE.load(Ordering::Acquire),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_assets_have_verified_metadata_and_hashes() {
        verify_assets().expect("bundled model hashes must remain stable");
        assert_eq!(
            BEAT_MODEL_ASSETS[0].license,
            "MIT (Beat This! / beat-this-rs)"
        );
        assert!(
            BEAT_MODEL_ASSETS
                .iter()
                .all(|asset| asset.origin.starts_with("https://"))
        );
    }

    #[test]
    fn initialization_and_model_execution_are_serialized() {
        // Keep this smoke test short; it also protects against accidentally
        // changing the singleton to per-request model construction.
        let samples = vec![0.0_f32; 22_050];
        let _ = analyze(&samples, 22_050);
        let (initializations, max_active) = test_inference_counters();
        assert_eq!(initializations, 1);
        assert_eq!(max_active, 1);
    }

    #[test]
    fn bundled_model_smoke_exposes_finite_raw_logits() {
        let sample_rate = 22_050_u32;
        let mut samples = vec![0.0_f32; sample_rate as usize * 8];
        for beat in 0..16 {
            let start = (0.5 * sample_rate as f32) as usize + beat * (sample_rate as usize / 2);
            for offset in 0..(sample_rate as usize / 40) {
                let index = start + offset;
                if let Some(sample) = samples.get_mut(index) {
                    let phase = offset as f32 / sample_rate as f32 * std::f32::consts::TAU * 80.0;
                    *sample = 0.8 * phase.sin();
                }
            }
        }
        let observations = analyze(&samples, sample_rate).expect("embedded model smoke input");
        assert!(!observations.is_empty());
        assert!(
            observations
                .beat_logits
                .iter()
                .chain(observations.downbeat_logits.iter())
                .all(|value| value.is_finite())
        );
    }

    #[test]
    fn concurrent_requests_share_one_serialized_model() {
        let samples = vec![0.0_f32; 22_050 * 2];
        std::thread::scope(|scope| {
            let first = scope.spawn(|| analyze(&samples, 22_050));
            let second = scope.spawn(|| analyze(&samples, 22_050));
            let _ = first.join();
            let _ = second.join();
        });
        let (initializations, max_active) = test_inference_counters();
        assert!(initializations <= 1);
        assert_eq!(max_active, 1);
    }

    #[test]
    fn failed_initialization_is_retryable_and_success_is_reused() {
        let mut state = TrackerState { tracker: None };
        let mut attempts = 0_u8;
        assert!(!ensure_tracker(&mut state, || {
            attempts += 1;
            Err("injected initialization failure".to_owned())
        }));
        assert!(state.tracker.is_none());
        assert!(ensure_tracker(&mut state, || {
            attempts += 1;
            let tracker: Tracker = Box::new(|_, _| None);
            Ok(tracker)
        }));
        assert_eq!(attempts, 2);
        assert!(ensure_tracker(&mut state, || {
            panic!("a successful tracker must be reused")
        }));
    }

    #[test]
    fn poisoned_tracker_mutex_is_recovered_without_panic() {
        let mutex = Mutex::new(TrackerState { tracker: None });
        let poisoned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = mutex.lock().unwrap();
            panic!("injected tracker mutex poison");
        }));
        assert!(poisoned.is_err());
        let mut state = lock_tracker(&mutex);
        assert!(ensure_tracker(&mut state, || {
            let tracker: Tracker = Box::new(|_, _| None);
            Ok(tracker)
        }));
    }
}
