use crate::config::Config;
use crate::dashboard;
use crate::fetch;
use crate::parser::PlayRequest;
use crate::queue::{Advanced, Queued, Queues};
use crate::resolve;
use crate::search;
use anyhow::Context;
use serenity::async_trait;
use serenity::model::channel::ChannelType;
use serenity::model::id::{ChannelId, GuildId, MessageId};
use serenity::prelude::Context as SerenityContext;
use songbird::Songbird;
use songbird::events::{Event, EventContext, EventHandler, TrackEvent};
use songbird::input::{File as FileInput, Input};
use songbird::tracks::TrackHandle;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;
use tokio::sync::Mutex as AsyncMutex;
use tokio::sync::mpsc;

/// Length of the acknowledgement tone.
const BEEP_DURATION: Duration = Duration::from_millis(150);
/// Songbird mixes at 48 kHz stereo, so the tone is encoded in that format
/// and needs no resampling.
const BEEP_SAMPLE_RATE: u32 = 48_000;
const BEEP_CHANNELS: u16 = 2;

/// Which acknowledgement tone to play back.
///
/// The two stages answer different questions -- "did it hear me?" and "is it
/// ready for what I am about to say?" -- and a speaker who says the next word
/// before the second tone finishes needs to tell them apart by ear. They are a
/// tone apart rather than a melody, so the pair stays recognisable as the same
/// acknowledgement instead of becoming something to wait for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    /// The wake word was heard. The first model is done listening.
    Wake,
    /// The larger model is loaded and ready for the request.
    Ready,
}

impl Tone {
    /// The tone's frequency in Hz: A5, then the next one up.
    pub fn frequency(self) -> f32 {
        match self {
            Self::Wake => 880.0,
            Self::Ready => 987.77,
        }
    }
}

/// Plays queued music and keeps the text channel dashboard in step.
pub struct Player {
    pub manager: Arc<Songbird>,
    /// The per-guild play queues, shared with the interaction handler.
    pub queues: Queues,
    /// Serialises the work for one guild.
    ///
    /// Two commands handled at once would each download and then start a
    /// track, and the second would replace the first mid-flight, so the song
    /// that was asked for would be heard as the one before it.
    busy: Busy,
    /// The track playing per guild, needed to pause and resume it.
    tracks: Tracks,
    /// Where each guild's dashboard lives, so a new song edits the existing
    /// message instead of posting another one beside it.
    dashboards: Dashboards,
    /// Resolves a spoken request to a real song before YouTube is searched.
    catalog: Arc<resolve::Catalog>,
    /// The "which one did you mean" buttons currently on offer.
    choices: Choices,
}

/// How many candidates the chooser offers.
const CHOICE_COUNT: usize = 3;

/// A pending question, and what each of its buttons would play.
///
/// The custom id on the buttons carries the video id, so this only has to
/// remember what to play for it -- a URL and a title, which a press cannot
/// recover on its own without another search.
#[derive(Debug, Clone)]
struct ChoiceEntry {
    candidates: Vec<search::Candidate>,
}

/// The chooser on offer per guild.
///
/// One outstanding question per guild: asking again replaces the previous
/// question, so this cannot grow with every ambiguous request, and a stale
/// button cannot be pressed into playing something the listener never chose.
#[derive(Debug, Clone, Default)]
struct Choices(Arc<Mutex<HashMap<GuildId, ChoiceEntry>>>);

impl Choices {
    fn set(&self, guild_id: GuildId, candidates: Vec<search::Candidate>) {
        self.lock().insert(guild_id, ChoiceEntry { candidates });
    }

    /// Takes the offered choice for a video, removing it so a second press of
    /// the same button does nothing.
    fn take_video(&self, guild_id: GuildId, video_id: &str) -> Option<search::Candidate> {
        let mut lock = self.lock();
        let entry = lock.get_mut(&guild_id)?;
        let index = entry
            .candidates
            .iter()
            .position(|c| c.video_id() == Some(video_id))?;
        let candidate = entry.candidates.remove(index);
        if entry.candidates.is_empty() {
            lock.remove(&guild_id);
        }
        Some(candidate)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<GuildId, ChoiceEntry>> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// One lock per guild, so a command in one guild cannot hold up another.
#[derive(Debug, Clone, Default)]
struct Busy(Arc<Mutex<HashMap<GuildId, Arc<AsyncMutex<()>>>>>);

impl Busy {
    fn get(&self, guild_id: GuildId) -> Arc<AsyncMutex<()>> {
        let mut locks = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        Arc::clone(locks.entry(guild_id).or_default())
    }
}

/// The playing track per guild.
///
/// Pausing and resuming go through a [`TrackHandle`] rather than the call, so
/// the handle has to be kept.
#[derive(Debug, Clone, Default)]
struct Tracks(Arc<Mutex<HashMap<GuildId, TrackHandle>>>);

impl Tracks {
    fn set(&self, guild_id: GuildId, handle: TrackHandle) {
        self.lock().insert(guild_id, handle);
    }

    fn get(&self, guild_id: GuildId) -> Option<TrackHandle> {
        self.lock().get(&guild_id).cloned()
    }

    fn take(&self, guild_id: GuildId) -> Option<TrackHandle> {
        self.lock().remove(&guild_id)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<GuildId, TrackHandle>> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// The channel and message id of each guild's dashboard.
///
/// Only the ids are kept; the dashboard is rebuilt from the queue every time,
/// so a cached copy of the message would go stale.
#[derive(Debug, Clone, Default)]
struct Dashboards(Arc<Mutex<HashMap<GuildId, (ChannelId, MessageId)>>>);

impl Dashboards {
    /// The dashboard message for a guild, if it has one.
    fn get(&self, guild_id: GuildId) -> Option<MessageId> {
        self.lock().get(&guild_id).map(|(_, id)| *id)
    }

    /// Records where a guild's dashboard lives.
    fn set(&self, guild_id: GuildId, channel_id: ChannelId, message_id: MessageId) {
        self.lock().insert(guild_id, (channel_id, message_id));
    }

    /// Forgets a guild's dashboard, so the next update posts a new one.
    fn forget(&self, guild_id: GuildId) {
        self.lock().remove(&guild_id);
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<GuildId, (ChannelId, MessageId)>> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl Player {
    pub fn new(manager: Arc<Songbird>, catalog: Arc<resolve::Catalog>) -> Self {
        Self {
            manager,
            queues: Queues::default(),
            busy: Busy::default(),
            tracks: Tracks::default(),
            dashboards: Dashboards::default(),
            catalog,
            choices: Choices::default(),
        }
    }

    /// Plays a short tone so the speaker knows the wake word and command were
    /// both understood, without stopping whatever is currently playing.
    ///
    /// The tone is given time to finish before the music starts, so it is not
    /// cut off by the song that follows. That wait happens even for the wake
    /// tone, which precedes a model load rather than a song: back to back tones
    /// would otherwise run into one another and read as a single beep.
    pub async fn acknowledge(&self, guild_id: GuildId, tone: Tone) -> anyhow::Result<()> {
        let call = self
            .manager
            .get(guild_id)
            .context("bot is not in a voice channel")?;
        {
            let mut handler = call.lock().await;
            handler.play_input(Input::from(beep_wav(tone.frequency())));
        }
        tokio::time::sleep(BEEP_DURATION + Duration::from_millis(50)).await;
        Ok(())
    }

    /// Finds the song the request means and adds it to the back of the
    /// guild's queue.
    ///
    /// The request is resolved against the catalog first, so the YouTube search
    /// is aimed at a known song rather than at whatever words were heard, and
    /// the results are scored against that song rather than by popularity. See
    /// [`crate::search`] for why the last point matters.
    ///
    /// When the best result is not convincing enough the bot posts the closest
    /// few in the alert channel and plays nothing, rather than picking a song
    /// the listener did not ask for.
    pub async fn play(
        &self,
        ctx: &SerenityContext,
        config: &Config,
        guild_id: GuildId,
        request: &PlayRequest,
    ) -> anyhow::Result<()> {
        let lock = self.busy.get(guild_id);
        let _guard = lock.lock().await;
        self.play_locked(ctx, config, guild_id, request).await
    }

    /// The body of [`Self::play`], for callers that already hold the lock.
    ///
    /// Split out because a chooser press goes through the same queue, dashboard
    /// and lock as a spoken request, and taking the lock twice would deadlock.
    async fn play_locked(
        &self,
        ctx: &SerenityContext,
        config: &Config,
        guild_id: GuildId,
        request: &PlayRequest,
    ) -> anyhow::Result<()> {
        // Checked before searching, so a full queue fails without a download.
        let queue = self.queues.get(guild_id);
        anyhow::ensure!(
            !queue.is_full(),
            "the queue is full, try again once a song finishes"
        );
        self.manager
            .get(guild_id)
            .context("bot is not in a voice channel")?;

        let choice = search::choose_match(
            request,
            &self.catalog,
            &search::YtDlpSearch,
            config.search_candidates,
            config.match_threshold,
            config.allow_unverified_autoplay,
        )
        .await
        .with_context(|| format!("no match for '{}'", request.query))?;

        let best = choice
            .best()
            .context("no playable song found for the request")?;

        if !choice.confident {
            self.offer_choice(ctx, config, guild_id, &choice).await;
            return Ok(());
        }

        self.enqueue(ctx, config, guild_id, &best.candidate).await
    }

    /// Posts the "which one did you mean" question in the alert channel.
    async fn offer_choice(
        &self,
        ctx: &SerenityContext,
        config: &Config,
        guild_id: GuildId,
        choice: &search::Choice,
    ) {
        let options: Vec<dashboard::ChoiceOption<'_>> = choice
            .top(CHOICE_COUNT)
            .iter()
            .filter_map(|scored| {
                let video_id = scored.candidate.video_id()?;
                Some(dashboard::ChoiceOption {
                    title: scored.candidate.title(),
                    channel: scored.candidate.channel(),
                    duration_secs: scored.candidate.duration,
                    video_id,
                })
            })
            .collect();
        if options.is_empty() {
            println!("[{guild_id}] no candidate had a video id, so there was nothing to offer");
            return;
        }

        let candidates: Vec<search::Candidate> = choice
            .top(CHOICE_COUNT)
            .iter()
            .map(|scored| scored.candidate.clone())
            .collect();
        self.choices.set(guild_id, candidates);

        let Some(channel_id) = resolve_alert_channel(ctx, config, guild_id) else {
            println!("[{guild_id}] no alert channel, so there was nowhere to ask");
            return;
        };
        // Built before the send so the borrowed label outlives the call.
        let resolved = choice.resolved.as_ref().map(|track| track.label());
        let message = dashboard::choice(&choice.requested, resolved.as_deref(), &options);
        if let Err(e) = channel_id.send_message(&ctx.http, message).await {
            println!("[{guild_id}] could not ask which song was meant: {e:#}");
        }
    }

    /// Plays one of the offered candidates.
    ///
    /// The candidate is looked up by video id rather than rebuilt from the
    /// button, so this cannot be pressed into playing something other than what
    /// was offered.
    pub async fn play_chosen(
        &self,
        ctx: &SerenityContext,
        config: &Config,
        guild_id: GuildId,
        video_id: &str,
    ) -> anyhow::Result<()> {
        let lock = self.busy.get(guild_id);
        let _guard = lock.lock().await;
        let candidate = self.choices.take_video(guild_id, video_id).with_context(
            || "that choice is no longer on offer; say the request again for new options",
        )?;
        self.enqueue(ctx, config, guild_id, &candidate).await
    }

    /// Downloads a chosen video and queues it, starting playback if idle.
    ///
    /// Unchanged from the pre-catalog path: the chooser only changes which video
    /// gets here, not what happens to it.
    async fn enqueue(
        &self,
        ctx: &SerenityContext,
        config: &Config,
        guild_id: GuildId,
        found: &search::Candidate,
    ) -> anyhow::Result<()> {
        let queue = self.queues.get(guild_id);

        // A video with no id cannot be cached or given a thumbnail, but the
        // audio is still worth playing.
        let video_id = found.video_id().unwrap_or_default().to_string();
        // Downloaded before the song joins the queue, so a failed fetch leaves
        // the queue and whatever is playing alone.
        let path = fetch::fetch(&video_id, &found.url, &fetch::cache_dir(guild_id.get()))
            .await
            .with_context(|| format!("could not fetch '{}'", found.title()))?;

        let song = Queued {
            path,
            title: found.title().to_string(),
            artist: found.channel().to_string(),
            url: found.url.clone(),
            video_id,
        };
        println!(
            "[{guild_id}] queued \u{201c}{}\u{201d} by {}",
            song.title, song.artist
        );

        // An empty queue means nothing is playing, so this is the first song
        // and should start rather than wait.
        let starting = queue.is_idle();
        anyhow::ensure!(queue.push(song), "the queue is full");

        if starting {
            self.start_next(ctx, config, guild_id).await;
        } else {
            self.draw_dashboard(ctx, config, guild_id).await;
        }
        Ok(())
    }

    /// Skips the song playing, starting the next one queued.
    pub async fn skip(&self, ctx: &SerenityContext, config: &Config, guild_id: GuildId) {
        let lock = self.busy.get(guild_id);
        let _guard = lock.lock().await;
        self.start_next(ctx, config, guild_id).await;
    }

    /// Pauses the current song, or resumes it when already paused.
    ///
    /// Returns whether the song is now paused, or `None` when nothing is
    /// playing to pause.
    pub async fn toggle_pause(&self, guild_id: GuildId) -> Option<bool> {
        let handle = self.tracks.get(guild_id)?;
        let queue = self.queues.get(guild_id);
        let paused = !queue.is_paused();
        let result = if paused {
            handle.pause()
        } else {
            handle.play()
        };
        if let Err(e) = result {
            println!("failed to change playback state: {e}");
            return None;
        }
        queue.set_paused(paused);
        Some(paused)
    }

    /// Stops playback and clears the queue.
    pub async fn stop(&self, ctx: &SerenityContext, config: &Config, guild_id: GuildId) {
        let lock = self.busy.get(guild_id);
        let _guard = lock.lock().await;
        if let Some(call) = self.manager.get(guild_id) {
            let mut handler = call.lock().await;
            handler.stop();
        }
        self.tracks.take(guild_id);
        self.queues.get(guild_id).clear();
        self.draw_dashboard(ctx, config, guild_id).await;
    }

    /// Moves the queue on by one song and plays it.
    ///
    /// The song that was playing is stopped, whether it ended on its own or
    /// was skipped.
    async fn start_next(&self, ctx: &SerenityContext, config: &Config, guild_id: GuildId) {
        let advanced = self.queues.get(guild_id).advance();
        if let Some(handle) = start_track(&self.manager, guild_id, &advanced).await {
            self.tracks.set(guild_id, handle.clone());
            self.follow_when_finished(ctx, config, guild_id, handle, advanced.generation);
        }
        self.draw_dashboard(ctx, config, guild_id).await;
    }

    /// Starts the next queued song once this one finishes on its own.
    ///
    /// Songbird reports a track ending both when it finishes and when it is
    /// stopped, so a skip fires this callback too. The generation check makes
    /// a superseded callback do nothing rather than drop a second song.
    fn follow_when_finished(
        &self,
        ctx: &SerenityContext,
        config: &Config,
        guild_id: GuildId,
        handle: TrackHandle,
        generation: u64,
    ) {
        let (tx, mut finished) = mpsc::unbounded_channel();
        // The handler runs on a songbird thread, so it only wakes the task
        // below rather than doing the work itself.
        let _ = handle.add_event(Event::Track(TrackEvent::End), Ended(tx));

        let this = Follower {
            manager: Arc::clone(&self.manager),
            queues: self.queues.clone(),
            tracks: self.tracks.clone(),
            dashboards: self.dashboards.clone(),
            busy: self.busy.clone(),
            ctx: ctx.clone(),
            config: config.clone(),
            guild_id,
        };
        tokio::spawn(async move {
            let mut generation = generation;
            while finished.recv().await.is_some() {
                // The same lock the commands take, so a song ending and a skip
                // arriving together settle in one order or the other instead
                // of interleaving.
                let lock = this.busy.get(guild_id);
                let _held = lock.lock().await;

                // A skip moves the queue on itself, so this song may already
                // have been replaced. The check keeps a superseded ending
                // harmless and gives the whole advance one atomic step.
                let Some(advanced) = this.queues.get(guild_id).advance_if_current(generation)
                else {
                    return;
                };
                // The next ending is checked against the advance that just
                // happened, not the one before it. Reusing the old number would
                // make the following song look superseded, and the queue would
                // stop after a single song.
                generation = advanced.generation;

                match start_track(&this.manager, guild_id, &advanced).await {
                    Some(handle) => {
                        this.tracks.set(guild_id, handle.clone());
                        // Watch the new song in turn, so the queue keeps
                        // moving until it runs dry.
                        let (tx, rx) = mpsc::unbounded_channel();
                        let _ = handle.add_event(Event::Track(TrackEvent::End), Ended(tx));
                        finished = rx;
                        // Drawn here rather than only after the loop: every
                        // advance starts a different song, so waiting until the
                        // whole queue drained left the dashboard naming the song
                        // that had already ended for as long as the next one
                        // played.
                        this.draw_dashboard().await;
                    }
                    None => break,
                }
            }
            this.draw_dashboard().await;
        });
    }

    /// Creates or updates the guild's dashboard in the alert channel.
    ///
    /// Safe to call when no channel is available; feedback is then skipped
    /// rather than failing the command.
    pub async fn draw_dashboard(&self, ctx: &SerenityContext, config: &Config, guild_id: GuildId) {
        draw(&self.dashboards, &self.queues, ctx, config, guild_id).await;
    }
}

/// The parts of the player a songbird ending needs, owned so the callback can
/// outlive the call that set it up.
struct Follower {
    manager: Arc<Songbird>,
    queues: Queues,
    tracks: Tracks,
    dashboards: Dashboards,
    busy: Busy,
    ctx: SerenityContext,
    config: Config,
    guild_id: GuildId,
}

impl Follower {
    /// Draws or updates this guild's dashboard.
    async fn draw_dashboard(&self) {
        draw(
            &self.dashboards,
            &self.queues,
            &self.ctx,
            &self.config,
            self.guild_id,
        )
        .await;
    }
}

/// Wakes the advancement task when a track ends.
///
/// Songbird runs handlers on its own thread, so this only sends on a channel;
/// the queue is advanced on a normal async task where it can be awaited.
struct Ended(mpsc::UnboundedSender<()>);

#[async_trait]
impl EventHandler for Ended {
    async fn act(&self, _ctx: &EventContext<'_>) -> Option<Event> {
        let _ = self.0.send(());
        None
    }
}

/// Stops the song that was playing and starts the next one an advance chose.
///
/// Returns `None` when the queue has run dry or the bot has left the voice
/// channel. The finished song's file is removed, which is safe because every
/// song has a file of its own and the cache keeps the newest few.
///
/// The call is locked with `await` rather than `blocking_lock`, which panics
/// when it is reached from inside a runtime thread.
async fn start_track(
    manager: &Songbird,
    guild_id: GuildId,
    advanced: &Advanced,
) -> Option<TrackHandle> {
    let started = match (manager.get(guild_id), &advanced.current) {
        (Some(call), Some(song)) => {
            let mut handler = call.lock().await;
            handler.stop();
            println!("[{guild_id}] playing \u{201c}{}\u{201d}", song.title);
            let path = song.path.clone();
            Some(handler.play_input(Input::from(FileInput::new(path))))
        }
        _ => None,
    };
    if let Some(song) = &advanced.stopped {
        let _ = std::fs::remove_file(&song.path);
    }
    started
}

/// Draws or updates a guild's dashboard message.
async fn draw(
    dashboards: &Dashboards,
    queues: &Queues,
    ctx: &SerenityContext,
    config: &Config,
    guild_id: GuildId,
) {
    let Some(channel) = resolve_alert_channel(ctx, config, guild_id) else {
        return;
    };
    let queue = queues.get(guild_id);
    let current = queue.current();
    let view = match &current {
        Some(song) => dashboard::dashboard(song, queue.is_paused(), queue.pending()),
        None => dashboard::idle_dashboard(),
    };

    match dashboards.get(guild_id) {
        Some(message_id) => {
            if let Err(e) = channel
                .edit_message(&ctx.http, message_id, view.clone().into_edit())
                .await
            {
                // The message was deleted or access was lost, so post a new
                // dashboard rather than going quiet for the rest of the
                // session.
                println!("failed to update dashboard: {e:#}");
                dashboards.forget(guild_id);
                if let Ok(sent) = channel.send_message(&ctx.http, view.into_message()).await {
                    dashboards.set(guild_id, channel, sent.id);
                }
            }
        }
        None => match channel.send_message(&ctx.http, view.into_message()).await {
            Ok(sent) => dashboards.set(guild_id, channel, sent.id),
            Err(e) => println!("failed to post dashboard: {e:#}"),
        },
    }
}

/// Builds the acknowledgement tone as an in-memory 16-bit PCM WAV.
///
/// Songbird accepts a byte buffer directly as an input, so the tone needs no
/// file on disk. The attack and release are faded to keep it from clicking.
fn beep_wav(frequency: f32) -> Vec<u8> {
    let frames = (BEEP_SAMPLE_RATE as f32 * BEEP_DURATION.as_secs_f32()).round() as usize;
    let bytes_per_frame = BEEP_CHANNELS as usize * 2;
    let data_len = frames * bytes_per_frame;
    let fade_frames = (BEEP_SAMPLE_RATE / 200) as f32;

    let mut wav = Vec::with_capacity(44 + data_len);
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&((36 + data_len) as u32).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes());
    wav.extend_from_slice(&BEEP_CHANNELS.to_le_bytes());
    wav.extend_from_slice(&BEEP_SAMPLE_RATE.to_le_bytes());
    let byte_rate = BEEP_SAMPLE_RATE as usize * bytes_per_frame;
    wav.extend_from_slice(&(byte_rate as u32).to_le_bytes());
    wav.extend_from_slice(&(bytes_per_frame as u16).to_le_bytes());
    wav.extend_from_slice(&16u16.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&(data_len as u32).to_le_bytes());

    let step = 2.0 * std::f32::consts::PI * frequency / BEEP_SAMPLE_RATE as f32;
    for frame in 0..frames {
        let envelope = (frame as f32 / fade_frames)
            .min((frames - 1 - frame) as f32 / fade_frames)
            .clamp(0.0, 1.0);
        let tone = (step * frame as f32).sin() * i16::MAX as f32;
        let sample = (envelope * 0.3 * tone) as i16;
        for _ in 0..BEEP_CHANNELS {
            wav.extend_from_slice(&sample.to_le_bytes());
        }
    }
    wav
}

/// Picks the text channel used for feedback: the configured alert channel,
/// else the guild's system channel, else the first text channel.
fn resolve_alert_channel(
    ctx: &SerenityContext,
    config: &Config,
    guild_id: GuildId,
) -> Option<ChannelId> {
    if let Some(id) = config.alert_channel_id {
        return Some(ChannelId::new(id));
    }
    let guild = ctx.cache.guild(guild_id)?;
    guild.system_channel_id.or_else(|| {
        guild
            .channels
            .values()
            .find(|channel| channel.kind == ChannelType::Text)
            .map(|channel| channel.id)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reads a little-endian `u32` at `offset`.
    fn u32_at(wav: &[u8], offset: usize) -> u32 {
        wav[offset..offset + 4]
            .try_into()
            .map(u32::from_le_bytes)
            .unwrap()
    }

    /// Reads a little-endian `u16` at `offset`.
    fn u16_at(wav: &[u8], offset: usize) -> u16 {
        wav[offset..offset + 2]
            .try_into()
            .map(u16::from_le_bytes)
            .unwrap()
    }

    /// The tone has to survive the same probe songbird runs on it.
    ///
    /// Songbird depends on symphonia with `default-features = false` and exposes
    /// no feature to switch the format handlers back on, so its probe registry
    /// is empty unless symphonia is also a direct dependency of this crate. That
    /// build decodes nothing at all: every input fails with "no suitable format
    /// reader found", so the tone is silent and the music never starts.
    #[test]
    fn songbird_can_probe_and_decode_the_tone() {
        use symphonia::core::codecs::DecoderOptions;
        use symphonia::core::formats::FormatOptions;
        use symphonia::core::io::MediaSourceStream;
        use symphonia::core::meta::MetadataOptions;
        use symphonia::default::{get_codecs, get_probe};

        let wav = beep_wav(Tone::Wake.frequency());
        let source =
            MediaSourceStream::new(Box::new(std::io::Cursor::new(wav)), Default::default());
        let mut probed = get_probe()
            .format(
                &symphonia::core::probe::Hint::new(),
                source,
                &FormatOptions::default(),
                &MetadataOptions::default(),
            )
            .expect("songbird's probe must recognise the tone");
        let track = probed
            .format
            .default_track()
            .expect("the tone must expose a track");
        let mut decoder = get_codecs()
            .make(&track.codec_params, &DecoderOptions::default())
            .expect("the pcm decoder must be registered");

        let mut packets = 0;
        while let Ok(packet) = probed.format.next_packet() {
            decoder.decode(&packet).expect("pcm packet must decode");
            packets += 1;
        }
        assert!(packets > 0, "the tone must decode to audio samples");
    }

    #[test]
    fn beep_is_a_playable_wav_header() {
        let wav = beep_wav(Tone::Wake.frequency());
        assert_eq!(&wav[0..4], b"RIFF");
        assert_eq!(&wav[8..12], b"WAVE");
        assert_eq!(&wav[12..16], b"fmt ");
        assert_eq!(u32_at(&wav, 16), 16, "PCM fmt chunk size");
        assert_eq!(u16_at(&wav, 20), 1, "uncompressed PCM");
        assert_eq!(u16_at(&wav, 22), BEEP_CHANNELS);
        assert_eq!(u32_at(&wav, 24), BEEP_SAMPLE_RATE);
        assert_eq!(u16_at(&wav, 34), 16, "bits per sample");
        assert_eq!(&wav[36..40], b"data");
        let data_len = u32_at(&wav, 40) as usize;
        assert_eq!(wav.len(), 44 + data_len, "declared data length");
        assert_eq!(u32_at(&wav, 4) as usize, 36 + data_len, "RIFF length");
    }

    #[test]
    fn beep_holds_audible_tone_for_the_requested_duration() {
        let wav = beep_wav(Tone::Wake.frequency());
        let samples: Vec<i16> = wav[44..]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| i16::from_le_bytes(*c))
            .collect();
        let frames = samples.len() / BEEP_CHANNELS as usize;
        let expected = (BEEP_SAMPLE_RATE as f32 * BEEP_DURATION.as_secs_f32()) as usize;
        assert!(
            (frames as i32 - expected as i32).abs() <= 1,
            "{frames} frames"
        );

        let peak = samples.iter().map(|s| s.unsigned_abs()).max().unwrap();
        assert!(peak > i16::MAX as u16 / 5, "tone is too quiet: peak {peak}");

        let left = samples.iter().step_by(2);
        let right = samples.iter().skip(1).step_by(2);
        assert!(
            left.eq(right),
            "both channels carry the same tone, so it plays centred"
        );
    }

    fn candidate(url: &str) -> search::Candidate {
        search::Candidate {
            title: Some("Dracula".to_string()),
            url: url.to_string(),
            view_count: Some(1),
            duration: Some(206.0),
            channel: Some("Tame Impala".to_string()),
            id: None,
        }
    }

    /// A press has to find the video it stands for among the offered ones.
    #[test]
    fn a_press_finds_the_video_it_stands_for() {
        let choices = Choices::default();
        let guild = GuildId::new(7);
        choices.set(
            guild,
            vec![
                candidate("https://youtu.be/first"),
                candidate("https://youtu.be/second"),
            ],
        );
        let picked = choices.take_video(guild, "second").expect("was offered");
        assert_eq!(picked.url, "https://youtu.be/second");
    }

    /// The buttons are spent by a press, so a second one must not replay the
    /// song.
    #[test]
    fn a_press_is_spent_once() {
        let choices = Choices::default();
        let guild = GuildId::new(7);
        choices.set(guild, vec![candidate("https://youtu.be/only")]);
        assert!(choices.take_video(guild, "only").is_some());
        assert!(choices.take_video(guild, "only").is_none());
    }

    /// A video that was never offered must not be playable, or the button
    /// could be crafted into playing something else.
    #[test]
    fn a_video_that_was_never_offered_is_refused() {
        let choices = Choices::default();
        let guild = GuildId::new(7);
        choices.set(guild, vec![candidate("https://youtu.be/offered")]);
        assert!(choices.take_video(guild, "smuggled").is_none());
    }

    /// Asking again replaces the question, so a stale button cannot be pressed
    /// into playing something from a request that has since been forgotten.
    #[test]
    fn a_second_question_replaces_the_first() {
        let choices = Choices::default();
        let guild = GuildId::new(7);
        choices.set(guild, vec![candidate("https://youtu.be/old")]);
        choices.set(guild, vec![candidate("https://youtu.be/new")]);
        assert!(choices.take_video(guild, "old").is_none());
        assert!(choices.take_video(guild, "new").is_some());
    }

    /// One question per guild, so the map cannot grow with every ambiguous
    /// request, and a press in one guild cannot play in another.
    #[test]
    fn questions_are_kept_per_guild() {
        let choices = Choices::default();
        choices.set(GuildId::new(1), vec![candidate("https://youtu.be/one")]);
        choices.set(GuildId::new(2), vec![candidate("https://youtu.be/two")]);
        assert_eq!(choices.take_video(GuildId::new(2), "one"), None);
        assert_eq!(
            choices.take_video(GuildId::new(1), "one").unwrap().url,
            "https://youtu.be/one"
        );
        // The untouched guild is unaffected by the other guild's press.
        assert_eq!(
            choices.take_video(GuildId::new(2), "two").unwrap().url,
            "https://youtu.be/two"
        );
    }

    /// Once every button is spent the question is forgotten, rather than
    /// leaving an empty husk behind for the guild.
    #[test]
    fn a_fully_spent_question_is_forgotten() {
        let choices = Choices::default();
        let guild = GuildId::new(7);
        choices.set(guild, vec![candidate("https://youtu.be/only")]);
        assert!(choices.take_video(guild, "only").is_some());
        assert!(choices.lock().is_empty());
    }

    #[test]
    fn beep_fades_in_and_out_instead_of_clicking() {
        let wav = beep_wav(Tone::Wake.frequency());
        let samples: Vec<i16> = wav[44..]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| i16::from_le_bytes(*c))
            .collect();
        let peak = samples.iter().map(|s| s.unsigned_abs()).max().unwrap();
        let first = samples[0].unsigned_abs();
        let last = samples[samples.len() - 2].unsigned_abs();
        assert!(first < peak / 4, "starts at {first}, peak {peak}");
        assert!(last < peak / 4, "ends at {last}, peak {peak}");
    }

    /// The frequency a tone actually contains, counted from its zero crossings.
    ///
    /// A sine crosses zero twice per cycle, so the crossing rate over the
    /// measured span is twice its frequency. Two details make the count honest:
    /// the span is the middle 80%, because the fades at each end are what stops
    /// the tone clicking; and exact zeros are dropped first, because truncating
    /// the tone into i16 rounds the samples either side of a crossing to zero,
    /// and a zero next to a positive and then next to a negative reads as two
    /// crossings where the signal only makes one.
    fn frequency_of(wav: &[u8]) -> f32 {
        let samples: Vec<i16> = wav[44..]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| i16::from_le_bytes(*c))
            .collect();
        let middle = &samples[samples.len() / 10..samples.len() * 9 / 10];
        let measured: Vec<i16> = middle.iter().copied().filter(|s| *s != 0).collect();
        let crossings = measured
            .windows(2)
            .filter(|w| w[0].signum() != w[1].signum())
            .count();
        crossings as f32 / 2.0 / (BEEP_DURATION.as_secs_f32() * 0.8)
    }

    /// The second tone has to be told apart from the first by ear alone. If the
    /// two were the same, or too close to separate, a speaker who starts their
    /// request too early would have no way to tell which stage answered them.
    #[test]
    fn the_two_acknowledgement_tones_are_distinguishable() {
        let wake = frequency_of(&beep_wav(Tone::Wake.frequency()));
        let ready = frequency_of(&beep_wav(Tone::Ready.frequency()));

        assert!(
            (wake - Tone::Wake.frequency()).abs() < 20.0,
            "wake tone measured {wake} Hz"
        );
        assert!(
            (ready - Tone::Ready.frequency()).abs() < 20.0,
            "ready tone measured {ready} Hz"
        );
        assert!(
            ready - wake > 60.0,
            "{wake} Hz and {ready} Hz are too close to tell apart"
        );
    }

    /// The tone the second stage plays has to decode like the first, or a model
    /// that is ready would be announced in silence.
    #[test]
    fn the_ready_tone_decodes_too() {
        use symphonia::core::codecs::DecoderOptions;
        use symphonia::core::formats::FormatOptions;
        use symphonia::core::io::MediaSourceStream;
        use symphonia::core::meta::MetadataOptions;
        use symphonia::default::{get_codecs, get_probe};

        let wav = beep_wav(Tone::Ready.frequency());
        let source =
            MediaSourceStream::new(Box::new(std::io::Cursor::new(wav)), Default::default());
        let mut probed = get_probe()
            .format(
                &symphonia::core::probe::Hint::new(),
                source,
                &FormatOptions::default(),
                &MetadataOptions::default(),
            )
            .expect("songbird's probe must recognise the ready tone");
        let track = probed.format.default_track().expect("a track");
        let mut decoder = get_codecs()
            .make(&track.codec_params, &DecoderOptions::default())
            .expect("the pcm decoder must be registered");

        let mut packets = 0;
        while let Ok(packet) = probed.format.next_packet() {
            decoder.decode(&packet).expect("pcm packet must decode");
            packets += 1;
        }
        assert!(packets > 0, "the ready tone must decode to audio samples");
    }
}
