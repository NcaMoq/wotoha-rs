use std::{
    collections::HashMap,
    error::Error,
    fmt,
    fs::OpenOptions,
    io::{self, Write},
    sync::{Arc, Mutex},
    time::Duration,
};

use serenity::{all::GatewayIntents, async_trait, client::Client};
use songbird::SerenityInit;
use tracing::{info, level_filters::LevelFilter};
use tracing_appender::non_blocking;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::MakeWriter;
use wotoha_contracts::{
    ChannelKey, EnqueueOutcome, GuildKey, PlaybackId, PlaybackService, RuntimeEventSink,
    RuntimeTrackHandle, UserKey, VoiceActionAccess, VoiceGatewayEvent, VoiceGatewayRuntime,
    VoicePeerSnapshot, VoiceRuntime, VoiceUpdateDecision,
};
use wotoha_control::ControlService;
use wotoha_core::{
    BotConfig, QueuePreview, TrackRequest,
    config::PlaybackConfig,
    debug::{append_debug_log, sanitize_log_message},
};
use wotoha_media::MediaResolver;
use wotoha_runtime::{DiscordGateway, SongbirdRuntime, recommended_cache_settings};
use wotoha_voice::PlaybackCoordinator;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let config = BotConfig::load()?;
    std::fs::create_dir_all(&config.logging.directory)?;
    let log_path = config.logging.file_path();
    let log_file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)?;
    let (file_writer, guard) = non_blocking(log_file);
    let _guard = Box::leak(Box::new(guard));
    let env_filter = EnvFilter::builder()
        .with_default_directive(LevelFilter::INFO.into())
        .parse(&config.logging.rust_log)?
        .add_directive("symphonia_format_isomp4=error".parse()?);
    tracing_subscriber::fmt()
        .with_env_filter(env_filter)
        .with_ansi(config.logging.ansi)
        .with_writer(SanitizingMakeWriter::new(DualMakeWriter::new(
            file_writer,
            std::io::stdout,
        )))
        .init();
    append_debug_log("main: boot starting");
    append_debug_log("main: config loaded");
    info!(
        log_dir = %config.logging.directory.display(),
        log_file = %config.logging.file_name,
        log_ansi = config.logging.ansi,
        default_volume = config.playback.default_volume,
        max_queue_len = config.playback.max_queue_len,
        max_pending_enqueues = config.playback.max_pending_enqueues,
        "configuration loaded"
    );
    let resolver = MediaResolver::new()?;
    append_debug_log("main: media resolver created");
    let resolver_warmup = resolver.clone();
    let warmup_task = tokio::spawn(async move {
        append_debug_log("main: media provider warmup starting");
        resolver_warmup.warmup_providers().await;
        append_debug_log("main: media provider warmup finished");
    });
    let (playback_runtime, songbird) = SongbirdRuntime::paired()?;
    let playback_runtime =
        ConfiguredVoiceRuntime::new(playback_runtime, config.playback.default_volume);
    append_debug_log("main: playback runtime created");
    let playback = PlaybackCoordinator::new(resolver, playback_runtime.clone());
    let playback = ConfiguredPlayback::new(playback, config.playback.clone());
    let control = ControlService::new(playback);
    let handler = DiscordGateway::new(control, playback_runtime);
    let shutdown_handler = handler.clone();
    let intents = GatewayIntents::GUILDS | GatewayIntents::GUILD_VOICE_STATES;
    let mut client = Client::builder(config.discord_token, intents)
        .cache_settings(recommended_cache_settings())
        .event_handler(handler)
        .register_songbird_with(songbird)
        .await?;
    let http = client.http.clone();
    let shard_manager = client.shard_manager.clone();
    append_debug_log("main: serenity client built");
    if let Err(error) = warmup_task.await {
        append_debug_log(format!("main: media provider warmup task failed: {error}"));
    }

    append_debug_log("main: starting client");
    tokio::select! {
        result = client.start() => result?,
        result = shutdown_signal() => {
            result?;
            append_debug_log("main: shutdown signal received; notifying active sessions");
            shutdown_handler.notify_restart(&http).await;
            tokio::time::sleep(Duration::from_secs(3)).await;
            shard_manager.shutdown_all().await;
        }
    }
    append_debug_log("main: client exited");
    Ok(())
}

async fn shutdown_signal() -> io::Result<()> {
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        tokio::select! {
            result = tokio::signal::ctrl_c() => result,
            _ = terminate.recv() => Ok(()),
        }
    }

    #[cfg(not(unix))]
    tokio::signal::ctrl_c().await
}

#[derive(Clone)]
struct ConfiguredVoiceRuntime<R> {
    inner: R,
    default_volume: f32,
}

impl<R> ConfiguredVoiceRuntime<R> {
    fn new(inner: R, default_volume: f32) -> Self {
        Self {
            inner,
            default_volume,
        }
    }
}

#[async_trait]
impl<R> VoiceRuntime for ConfiguredVoiceRuntime<R>
where
    R: VoiceRuntime,
{
    type Error = R::Error;

    async fn play_track(
        &self,
        guild_id: GuildKey,
        session_id: u64,
        playback_id: PlaybackId,
        request: &TrackRequest,
        events: RuntimeEventSink,
    ) -> Result<Arc<dyn RuntimeTrackHandle>, Self::Error> {
        let handle = self
            .inner
            .play_track(guild_id, session_id, playback_id, request, events)
            .await?;
        Ok(Arc::new(ConfiguredTrackHandle::new(
            handle,
            self.default_volume,
        )))
    }

    async fn disconnect_guild(&self, guild_id: GuildKey) -> Result<(), Self::Error> {
        self.inner.disconnect_guild(guild_id).await
    }
}

#[async_trait]
impl<R> VoiceGatewayRuntime for ConfiguredVoiceRuntime<R>
where
    R: VoiceGatewayRuntime,
{
    async fn ensure_joined(
        &self,
        guild_id: GuildKey,
        channel_id: ChannelKey,
    ) -> Result<bool, Self::Error> {
        self.inner.ensure_joined(guild_id, channel_id).await
    }

    async fn handle_gateway_event(&self, event: VoiceGatewayEvent) -> Result<(), Self::Error> {
        self.inner.handle_gateway_event(event).await
    }
}

struct ConfiguredTrackHandle {
    inner: Arc<dyn RuntimeTrackHandle>,
    default_volume: f32,
}

impl ConfiguredTrackHandle {
    fn new(inner: Arc<dyn RuntimeTrackHandle>, default_volume: f32) -> Self {
        Self {
            inner,
            default_volume,
        }
    }
}

impl RuntimeTrackHandle for ConfiguredTrackHandle {
    fn stop(&self) {
        self.inner.stop();
    }

    fn set_volume(&self, _volume: f32) {
        self.inner.set_volume(self.default_volume);
    }
}

#[derive(Clone)]
struct ConfiguredPlayback<P> {
    inner: P,
    config: PlaybackConfig,
    pending_by_guild: Arc<Mutex<HashMap<GuildKey, usize>>>,
}

impl<P> ConfiguredPlayback<P> {
    fn new(inner: P, config: PlaybackConfig) -> Self {
        Self {
            inner,
            config,
            pending_by_guild: Arc::default(),
        }
    }
}

impl<P> ConfiguredPlayback<P>
where
    P: PlaybackService,
{
    fn reserve_enqueue(
        &self,
        guild_id: GuildKey,
    ) -> Result<PendingReservation, ConfiguredPlaybackError<P::Error>> {
        let queued = self
            .inner
            .queue_preview(guild_id, 0)
            .map(|preview| preview.total_queued())
            .unwrap_or(0);
        let mut pending_by_guild = self
            .pending_by_guild
            .lock()
            .expect("configured playback pending counters");
        let pending = pending_by_guild.get(&guild_id).copied().unwrap_or(0);
        if queued + pending >= self.config.max_queue_len
            || pending >= self.config.max_pending_enqueues
        {
            return Err(ConfiguredPlaybackError::QueueFull {
                max_queue_len: self.config.max_queue_len,
                max_pending_enqueues: self.config.max_pending_enqueues,
            });
        }

        pending_by_guild.insert(guild_id, pending + 1);
        Ok(PendingReservation::new(
            guild_id,
            Arc::clone(&self.pending_by_guild),
        ))
    }
}

struct PendingReservation {
    guild_id: GuildKey,
    pending_by_guild: Arc<Mutex<HashMap<GuildKey, usize>>>,
}

impl PendingReservation {
    fn new(guild_id: GuildKey, pending_by_guild: Arc<Mutex<HashMap<GuildKey, usize>>>) -> Self {
        Self {
            guild_id,
            pending_by_guild,
        }
    }
}

impl Drop for PendingReservation {
    fn drop(&mut self) {
        let mut pending_by_guild = self
            .pending_by_guild
            .lock()
            .expect("configured playback pending counters");
        match pending_by_guild.get_mut(&self.guild_id) {
            Some(count) if *count > 1 => *count -= 1,
            Some(_) => {
                pending_by_guild.remove(&self.guild_id);
            }
            None => {}
        }
    }
}

#[derive(Debug)]
enum ConfiguredPlaybackError<E> {
    Inner(E),
    QueueFull {
        max_queue_len: usize,
        max_pending_enqueues: usize,
    },
}

impl<E> fmt::Display for ConfiguredPlaybackError<E>
where
    E: fmt::Display,
{
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Inner(error) => error.fmt(out),
            Self::QueueFull {
                max_queue_len,
                max_pending_enqueues,
            } => write!(
                out,
                "queue is full for this guild (configured limits: max_queue_len={max_queue_len}, max_pending_enqueues={max_pending_enqueues})"
            ),
        }
    }
}

impl<E> Error for ConfiguredPlaybackError<E>
where
    E: Error + 'static,
{
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Inner(error) => Some(error),
            Self::QueueFull { .. } => None,
        }
    }
}

#[async_trait]
impl<P> PlaybackService for ConfiguredPlayback<P>
where
    P: PlaybackService,
{
    type Error = ConfiguredPlaybackError<P::Error>;

    async fn enqueue(
        &self,
        guild_id: GuildKey,
        source_url: &str,
    ) -> Result<EnqueueOutcome, Self::Error> {
        let _reservation = self.reserve_enqueue(guild_id)?;
        self.inner
            .enqueue(guild_id, source_url)
            .await
            .map_err(ConfiguredPlaybackError::Inner)
    }

    fn queue_preview(&self, guild_id: GuildKey, limit: usize) -> Option<QueuePreview> {
        self.inner.queue_preview(guild_id, limit)
    }

    async fn toggle_loop(&self, guild_id: GuildKey) -> Option<bool> {
        self.inner.toggle_loop(guild_id).await
    }

    async fn skip(&self, guild_id: GuildKey) -> Option<bool> {
        self.inner.skip(guild_id).await
    }

    fn has_current_track(&self, guild_id: GuildKey) -> bool {
        self.inner.has_current_track(guild_id)
    }

    async fn shuffle(&self, guild_id: GuildKey) -> bool {
        self.inner.shuffle(guild_id).await
    }

    async fn disconnect_guild(&self, guild_id: GuildKey) {
        self.inner.disconnect_guild(guild_id).await;
    }

    fn bootstrap_voice_state(
        &self,
        guild_id: GuildKey,
        bot_channel: ChannelKey,
        peers: Vec<VoicePeerSnapshot>,
    ) {
        self.inner
            .bootstrap_voice_state(guild_id, bot_channel, peers);
    }

    fn update_bot_voice_channel(&self, guild_id: GuildKey, new_channel: Option<ChannelKey>) {
        self.inner.update_bot_voice_channel(guild_id, new_channel);
    }

    fn clear_voice_state(&self, guild_id: GuildKey) {
        self.inner.clear_voice_state(guild_id);
    }

    fn apply_peer_voice_state(
        &self,
        guild_id: GuildKey,
        user_id: UserKey,
        old_channel: Option<ChannelKey>,
        new_channel: Option<ChannelKey>,
    ) -> VoiceUpdateDecision {
        self.inner
            .apply_peer_voice_state(guild_id, user_id, old_channel, new_channel)
    }

    fn voice_action_access(
        &self,
        guild_id: GuildKey,
        actor_channel: Option<ChannelKey>,
    ) -> VoiceActionAccess {
        self.inner.voice_action_access(guild_id, actor_channel)
    }
}

struct DualMakeWriter<A, B> {
    left: A,
    right: B,
}

impl<A, B> DualMakeWriter<A, B> {
    fn new(left: A, right: B) -> Self {
        Self { left, right }
    }
}

impl<'a, A, B> MakeWriter<'a> for DualMakeWriter<A, B>
where
    A: MakeWriter<'a>,
    B: MakeWriter<'a>,
{
    type Writer = DualWriter<A::Writer, B::Writer>;

    fn make_writer(&'a self) -> Self::Writer {
        DualWriter::new(self.left.make_writer(), self.right.make_writer())
    }
}

struct DualWriter<A, B> {
    left: A,
    right: B,
}

impl<A, B> DualWriter<A, B> {
    fn new(left: A, right: B) -> Self {
        Self { left, right }
    }
}

impl<A, B> Write for DualWriter<A, B>
where
    A: Write,
    B: Write,
{
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.left.write_all(buf)?;
        self.right.write_all(buf)?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.left.flush()?;
        self.right.flush()
    }
}

struct SanitizingMakeWriter<W> {
    inner: W,
}

impl<W> SanitizingMakeWriter<W> {
    fn new(inner: W) -> Self {
        Self { inner }
    }
}

impl<'a, W> MakeWriter<'a> for SanitizingMakeWriter<W>
where
    W: MakeWriter<'a>,
{
    type Writer = SanitizingWriter<W::Writer>;

    fn make_writer(&'a self) -> Self::Writer {
        SanitizingWriter::new(self.inner.make_writer())
    }
}

struct SanitizingWriter<W> {
    inner: W,
    buffered: Vec<u8>,
}

impl<W> SanitizingWriter<W>
where
    W: Write,
{
    fn new(inner: W) -> Self {
        Self {
            inner,
            buffered: Vec::new(),
        }
    }

    fn flush_complete_lines(&mut self) -> io::Result<()> {
        while let Some(position) = self.buffered.iter().position(|byte| *byte == b'\n') {
            let line = self.buffered.drain(..=position).collect::<Vec<_>>();
            let content = &line[..line.len().saturating_sub(1)];
            self.write_sanitized(content, true)?;
        }
        Ok(())
    }

    fn flush_buffered_tail(&mut self) -> io::Result<()> {
        if self.buffered.is_empty() {
            return Ok(());
        }

        let tail = std::mem::take(&mut self.buffered);
        self.write_sanitized(&tail, false)
    }

    fn write_sanitized(&mut self, bytes: &[u8], add_newline: bool) -> io::Result<()> {
        let message = String::from_utf8_lossy(bytes);
        let sanitized = sanitize_log_message(message.as_ref());
        self.inner.write_all(sanitized.as_bytes())?;
        if add_newline {
            self.inner.write_all(b"\n")?;
        }
        Ok(())
    }
}

impl<W> Write for SanitizingWriter<W>
where
    W: Write,
{
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.buffered.extend_from_slice(buf);
        self.flush_complete_lines()?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.flush_buffered_tail()?;
        self.inner.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ConfiguredPlayback, ConfiguredPlaybackError, ConfiguredTrackHandle, PendingReservation,
    };
    use async_trait::async_trait;
    use std::{
        error::Error,
        fmt,
        sync::{
            Arc, Mutex,
            atomic::{AtomicUsize, Ordering},
        },
        time::Duration,
    };
    use wotoha_contracts::{
        ChannelKey, EnqueueOutcome, GuildKey, PlaybackService, RuntimeTrackHandle, UserKey,
        VoiceActionAccess, VoicePeerSnapshot, VoiceUpdateDecision,
    };
    use wotoha_core::{
        GuildPlayerState, PreparedSource, QueuePreview, TrackMetadata, TrackRequest,
        automix::AutoMixConfig, config::PlaybackConfig,
    };

    #[derive(Clone, Default)]
    struct MockPlayback {
        queue_preview: Arc<Mutex<Option<QueuePreview>>>,
        enqueue_calls: Arc<AtomicUsize>,
    }

    #[derive(Debug)]
    struct MockPlaybackError;

    impl fmt::Display for MockPlaybackError {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("mock playback error")
        }
    }

    impl Error for MockPlaybackError {}

    #[async_trait]
    impl PlaybackService for MockPlayback {
        type Error = MockPlaybackError;

        async fn enqueue(
            &self,
            _guild_id: GuildKey,
            source_url: &str,
        ) -> Result<EnqueueOutcome, Self::Error> {
            self.enqueue_calls.fetch_add(1, Ordering::SeqCst);
            Ok(EnqueueOutcome {
                now_playing: true,
                request: track_request(source_url),
            })
        }

        fn queue_preview(&self, _guild_id: GuildKey, _limit: usize) -> Option<QueuePreview> {
            self.queue_preview.lock().unwrap().clone()
        }

        async fn toggle_loop(&self, _guild_id: GuildKey) -> Option<bool> {
            None
        }

        async fn skip(&self, _guild_id: GuildKey) -> Option<bool> {
            None
        }

        fn has_current_track(&self, _guild_id: GuildKey) -> bool {
            false
        }

        async fn shuffle(&self, _guild_id: GuildKey) -> bool {
            false
        }

        async fn disconnect_guild(&self, _guild_id: GuildKey) {}

        fn bootstrap_voice_state(
            &self,
            _guild_id: GuildKey,
            _bot_channel: ChannelKey,
            _peers: Vec<VoicePeerSnapshot>,
        ) {
        }

        fn update_bot_voice_channel(&self, _guild_id: GuildKey, _new_channel: Option<ChannelKey>) {}

        fn clear_voice_state(&self, _guild_id: GuildKey) {}

        fn apply_peer_voice_state(
            &self,
            _guild_id: GuildKey,
            _user_id: UserKey,
            _old_channel: Option<ChannelKey>,
            _new_channel: Option<ChannelKey>,
        ) -> VoiceUpdateDecision {
            VoiceUpdateDecision::Ignore
        }

        fn voice_action_access(
            &self,
            _guild_id: GuildKey,
            _actor_channel: Option<ChannelKey>,
        ) -> VoiceActionAccess {
            VoiceActionAccess::NoActiveChannel
        }
    }

    struct MockTrackHandle {
        stopped: Arc<AtomicUsize>,
        volumes: Arc<Mutex<Vec<f32>>>,
    }

    impl RuntimeTrackHandle for MockTrackHandle {
        fn stop(&self) {
            self.stopped.fetch_add(1, Ordering::SeqCst);
        }

        fn set_volume(&self, volume: f32) {
            self.volumes.lock().unwrap().push(volume);
        }
    }

    fn playback_config(max_queue_len: usize, max_pending_enqueues: usize) -> PlaybackConfig {
        PlaybackConfig {
            default_volume: 0.25,
            max_queue_len,
            max_pending_enqueues,
            automix: AutoMixConfig {
                enabled: true,
                crossfade: Duration::from_secs(8),
                max_tempo_adjustment: 0.06,
                min_beat_confidence: 0.7,
            },
        }
    }

    fn track_request(key: &str) -> TrackRequest {
        let track_url = format!("https://example.invalid/{key}");
        TrackRequest::new(
            "test",
            key,
            track_url.clone(),
            track_url.clone(),
            format!("https://media.example.invalid/{key}.opus"),
            PreparedSource::http(
                format!("https://stream.example.invalid/{key}.opus"),
                Vec::new(),
                None,
                None,
            ),
            TrackMetadata::new(key, "tester", track_url, None, None),
        )
    }

    fn queued_preview(total_queued: usize) -> QueuePreview {
        let mut state = GuildPlayerState::default();
        state.enqueue(track_request("current"));
        for index in 0..total_queued {
            state.enqueue(track_request(&format!("queued-{index}")));
        }
        state.queue_preview(0)
    }

    #[tokio::test]
    async fn configured_playback_rejects_queue_limit_before_inner_enqueue() {
        let inner = MockPlayback::default();
        *inner.queue_preview.lock().unwrap() = Some(queued_preview(1));
        let playback = ConfiguredPlayback::new(inner.clone(), playback_config(1, 1));

        let result = playback.enqueue(GuildKey::new(1), "overflow").await;

        assert!(matches!(
            result,
            Err(ConfiguredPlaybackError::QueueFull { .. })
        ));
        assert_eq!(inner.enqueue_calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn pending_reservation_releases_counter_on_drop() {
        let inner = MockPlayback::default();
        let playback = ConfiguredPlayback::new(inner, playback_config(8, 1));
        let guild_id = GuildKey::new(2);
        let reservation = playback.reserve_enqueue(guild_id).unwrap();

        assert!(matches!(
            playback.reserve_enqueue(guild_id),
            Err(ConfiguredPlaybackError::QueueFull { .. })
        ));

        drop(reservation);

        let _next: PendingReservation = playback.reserve_enqueue(guild_id).unwrap();
    }

    #[test]
    fn configured_track_handle_applies_default_volume() {
        let stopped = Arc::new(AtomicUsize::new(0));
        let volumes = Arc::new(Mutex::new(Vec::new()));
        let inner = Arc::new(MockTrackHandle {
            stopped: stopped.clone(),
            volumes: volumes.clone(),
        });
        let handle = ConfiguredTrackHandle::new(inner, 0.25);

        handle.set_volume(1.0);
        handle.stop();

        assert_eq!(*volumes.lock().unwrap(), vec![0.25]);
        assert_eq!(stopped.load(Ordering::SeqCst), 1);
    }
}
