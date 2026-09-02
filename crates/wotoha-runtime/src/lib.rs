mod audio_decode;
mod automix_cache;
mod automix_preview;
mod beat_this_analysis;
mod cancellable_http;
mod discord;
mod embedded_rten;
mod hls_security;
mod neural_resampler;
mod niconico_hls;
mod ranged_http;
mod reconnect;
mod songbird;
mod tempo_stretch;
mod transition_dsp;
mod validated_hls;

pub use audio_decode::{AnalysisBackend, AnalysisOutcome};
pub use automix_cache::{
    ANALYSIS_CACHE_SCHEMA_VERSION, ANALYSIS_CACHE_V2_MAX_FILE_BYTES,
    ANALYSIS_CACHE_V2_SCHEMA_VERSION, ANALYSIS_CACHE_V2_UNKNOWN_SCORE, AnalysisCache,
    AnalysisCacheError, AnalysisCacheKey,
};
pub use automix_preview::{
    AutoMixPreview, AutoMixPreviewError, AutoMixPreviewRenderIssue, AutoMixPreviewRenderMetrics,
};
pub use beat_this_analysis::{
    BEAT_MODEL_ASSETS, BEAT_THIS_VCS, BeatModelAssetMetadata, MAX_NEURAL_DURATION,
    MAX_NEURAL_LOW_BAND_SAMPLES, MAX_NEURAL_SAMPLES, NEURAL_SAMPLE_RATE,
    adapt_legacy_track_analysis, analyze_neural_rhythm, analyze_neural_rhythm_with_fallback,
    model_assets, track_analysis_v2_from_legacy, track_analysis_v2_from_legacy_rhythm,
    track_analysis_v2_from_legacy_with_backend, track_analysis_v2_from_legacy_with_neural_rhythm,
};
pub use discord::{DiscordGateway, recommended_cache_settings};
pub use songbird::{SongbirdRuntime, SongbirdRuntimeError};
pub use wotoha_core::analysis::TrackAnalysisV2;
