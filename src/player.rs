use crate::config::Config;
use crate::dashboard;
use crate::fetch;
use crate::parser::PlayRequest;
use crate::queue::{Advanced, Queue, Queued, Queues};
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
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;
use tokio::sync::Mutex as AsyncMutex;
use tokio::sync::OwnedMutexGuard;
use tokio::sync::mpsc;

/// Length of the acknowledgement tone.
const BEEP_DURATION: Duration = Duration::from_millis(150);
/// Songbird mixes at 48 kHz stereo, so the tone is encoded in that format
/// and needs no resampling.
const BEEP_SAMPLE_RATE: u32 = 48_000;
const BEEP_CHANNELS: u16 = 2;

/// Which acknowledgement tone to play back.
///
/// The stages answer different questions -- "did it hear me?", "is it ready for
/// what I am about to say?", and "sorry, that did not become a song" -- and a
/// speaker who says the next word before the second tone finishes needs to tell
/// them apart by ear. They are a tone apart rather than a melody, so the set
/// stays recognisable as the same acknowledgement instead of becoming something
/// to wait for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    /// The wake word was heard. The first model is done listening.
    Wake,
    /// The larger model is loaded and ready for the request.
    Ready,
    /// Nothing became of the request: no match, no search result, or a fetch
    /// that failed.
    Miss,
}

impl Tone {
    /// The tone's frequency in Hz: A5, the next one up, and A4.
    ///
    /// The miss tone is *below* the other two rather than between or above
    /// them: a failure answer should not sound like the start of an
    /// acknowledgement, and dropping an octave from A5 to A4 is a larger,
    /// easier-to-hear gap than the semitone the ready tone sits above.
    pub fn frequency(self) -> f32 {
        match self {
            Self::Wake => 880.0,
            Self::Ready => 987.77,
            Self::Miss => 440.0,
        }
    }
}

/// What became of a request once it had been resolved and downloaded.
///
/// Carried out of [`Player::resolve_and_queue`] rather than logged inside it, so
/// the Discord-facing part of a request (drawing the dashboard, posting the
/// chooser) is separable from the part that decides which song it was.
#[derive(Debug)]
pub enum Outcome {
    /// The song joined the queue. `starting` says whether nothing was playing,
    /// in which case playback begins now rather than when the queue reaches it.
    Played { starting: bool },
    /// Nothing was convincing enough to play; the caller should offer these.
    Asked { tracks: Vec<resolve::CatalogTrack> },
    /// Uploads of one known song, none of which the scorer was sure enough
    /// about. Offering them is a choice between *recordings*, not between songs.
    Uploads(search::Choice),
    /// Nothing matched at all.
    Unknown,
    /// Playback was stopped while this was still downloading.
    Cancelled,
}

/// Why a request could not be turned into a song.
///
/// The speaker gets a tone and a message for each of these. Previously every
/// failure was a `println!` nobody in the voice channel can see, so the
/// complaint was silence: the person who said "play X" heard nothing back and
/// no idea whether the bot had not heard them or could not find the song.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Problem {
    /// Nothing in the catalog or on YouTube was convincing enough.
    NoMatch,
    /// The song was found but its audio could not be downloaded.
    FetchFailed,
    /// The queue had no room.
    QueueFull,
}

/// A failure, and what to tell the listener about it.
///
/// Carried as its own type rather than inferred from the message so the wording
/// of an error can change without changing which message the listener reads.
#[derive(Debug)]
pub struct Failure {
    pub problem: Problem,
    pub error: anyhow::Error,
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.error.fmt(f)
    }
}

impl Failure {
    fn new(problem: Problem, error: anyhow::Error) -> Self {
        Self { problem, error }
    }

    /// What the listener is told, in one place so every failure reads the same.
    ///
    /// Echoes back what was heard, because the two things a person needs in
    /// order to fix this are what the bot thought it heard and what to say
    /// instead.
    fn message(&self, heard: &str) -> String {
        match self.problem {
            Problem::NoMatch => format!(
                "I heard “{heard}”, but couldn’t match it to a song. \
                 Try “play <title> by <artist>”."
            ),
            Problem::FetchFailed => {
                format!("I found “{heard}” but couldn’t download it. Try again in a moment.")
            }
            Problem::QueueFull => format!(
                "The queue is full, so “{heard}” wasn’t added. \
                 Skip or stop something first, then ask again."
            ),
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
    /// How requests in flight and playback controls share this guild.
    coordination: Coordination,
    /// The track playing per guild, needed to pause and resume it.
    tracks: Tracks,
    /// Where each guild's dashboard lives, so a new song edits the existing
    /// message instead of posting another one beside it.
    dashboards: Dashboards,
    /// Resolves a spoken request to a real song before YouTube is searched.
    catalog: Arc<resolve::Catalog>,
    /// The "which one did you mean" buttons currently on offer.
    choices: Choices,
    /// What the bot remembers, or `None` when it has been given no place to
    /// remember in.
    pub library: Option<Arc<crate::library::Library>>,
    /// Queued songs waiting to be judged: kept, or skipped too fast to trust?
    pending: PendingLearn,
    /// When the song now playing started, per guild.
    started_at: StartedAt,
    /// Downloads the audio for a chosen video.
    fetch: Arc<dyn AudioFetcher>,
}

/// Turns a search result into a queued song.
///
/// Behind a trait so the download can be made slow, slow-and-failing, or
/// instant in tests: the bug this seam exists for is about *when* the download
/// happens relative to the locks, which cannot be observed with a real yt-dlp.
#[serenity::async_trait]
pub trait AudioFetcher: Send + Sync {
    /// Downloads `found` and returns the song to queue.
    ///
    /// `in_use` names the audio already playing or queued, which the download
    /// cache must not prune: songbird reads a queued song's file when it starts,
    /// so deleting it plays silence instead of the song.
    async fn fetch(
        &self,
        guild_id: GuildId,
        found: &search::Candidate,
        in_use: &[std::path::PathBuf],
    ) -> anyhow::Result<Queued>;
}

/// The real download, through yt-dlp.
pub struct YtDlpFetch;

#[serenity::async_trait]
impl AudioFetcher for YtDlpFetch {
    async fn fetch(
        &self,
        guild_id: GuildId,
        found: &search::Candidate,
        in_use: &[std::path::PathBuf],
    ) -> anyhow::Result<Queued> {
        // A video with no id cannot be cached or given a thumbnail, but the
        // audio is still worth playing.
        let video_id = found.video_id().unwrap_or_default().to_string();
        // Downloaded before the song joins the queue, so a failed fetch leaves
        // the queue and whatever is playing alone.
        let path = fetch::fetch(
            &video_id,
            &found.url,
            &fetch::cache_dir(guild_id.get()),
            in_use,
        )
        .await
        .with_context(|| format!("could not fetch '{}'", found.title()))?;

        Ok(Queued {
            path,
            title: found.title().to_string(),
            artist: found.channel().to_string(),
            url: found.url.clone(),
            video_id,
        })
    }
}

/// How many candidates the chooser offers.
const CHOICE_COUNT: usize = 3;

/// How long a chooser's buttons stay on offer.
///
/// A question left on offer forever is not a question. After this the message
/// stays readable and the buttons go, so a press from an hour ago cannot start
/// a download.
const CHOICE_TTL: Duration = Duration::from_secs(60);

/// How long a miss notice stays up before the bot cleans it away.
///
/// Long enough for the person who spoke to look over and read what went wrong;
/// short enough that the channel is not a wall of past failures.
const NOTICE_TTL: Duration = Duration::from_secs(30);

/// A pending question, and what each of its buttons would play.
///
/// The custom id on the buttons carries the video id, so this only has to
/// remember what to play for it -- a URL and a title, which a press cannot
/// recover on its own without another search.
#[derive(Debug, Clone)]
struct ChoiceEntry {
    /// The offered options, each with the opaque key its button carries.
    options: Vec<(String, Offer)>,
}

/// What one button on a chooser would play.
///
/// Two kinds, because there are two questions. A catalog question offers
/// *songs* — "Stand By Me" by Ben E. King or by Oasis — and pressing one means
/// going and finding that song's upload, so the button stores the track rather
/// than a video. A video question offers *uploads of one known song*, and
/// pressing one plays it directly.
#[derive(Debug, Clone)]
enum Offer {
    /// A specific upload, from a search nothing catalog-backed produced.
    Video(search::Candidate),
    /// A catalog song. The user has already chosen the song by pressing it, so
    /// its best upload is taken even when the upload's own score is modest: the
    /// hard part, deciding which song it is, has already been answered.
    Song {
        track: resolve::CatalogTrack,
        requested: String,
    },
}

/// The chooser on offer per guild.
///
/// One outstanding question per guild: asking again replaces the previous
/// question, so this cannot grow with every ambiguous request, and a stale
/// button cannot be pressed into playing something the listener never chose.
#[derive(Debug, Clone, Default)]
struct Choices(Arc<Mutex<HashMap<GuildId, ChoiceEntry>>>);

impl Choices {
    /// Puts a question on offer, replacing whatever was there.
    fn set(&self, guild_id: GuildId, options: Vec<(String, Offer)>) {
        self.lock().insert(guild_id, ChoiceEntry { options });
    }

    /// Takes the offered choice for `key`, removing it so a second press of the
    /// same button does nothing.
    ///
    /// The key is looked up rather than acted on, so a crafted button cannot be
    /// pressed into playing something that was not on offer.
    fn take(&self, guild_id: GuildId, key: &str) -> Option<Offer> {
        let mut lock = self.lock();
        let entry = lock.get_mut(&guild_id)?;
        let index = entry
            .options
            .iter()
            .position(|(offered, _)| offered == key)?;
        let (_, offer) = entry.options.remove(index);
        if entry.options.is_empty() {
            lock.remove(&guild_id);
        }
        Some(offer)
    }

    /// Throws a question away, for "None of these".
    fn clear(&self, guild_id: GuildId) -> bool {
        self.lock().remove(&guild_id).is_some()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<GuildId, ChoiceEntry>> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// The custom id a press carries, and the key it names in the choice store.
///
/// The index is part of the id so a stale press from an older question cannot
/// land on a different option of a newer one: the store is replaced wholesale,
/// and the key is only meaningful alongside the question that offered it.
fn song_key(index: usize) -> String {
    format!("song:{index}")
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

/// Per-guild coordination between the requests in flight and the playback
/// controls.
///
/// Split out of `Player` because `Songbird` cannot be constructed without a
/// voice connection, and the thing that went wrong here is entirely about which
/// lock is held when -- which needs no connection to test.
#[derive(Debug, Clone, Default)]
struct Coordination {
    /// Held for a whole request: the catalog lookup, the video search and the
    /// download. Two commands spoken at once would each start a track, and the
    /// second would replace the first mid-flight, so the song that was asked
    /// for would be heard as the one before it. It is FIFO, so requests are
    /// served in the order they were spoken.
    order: Busy,
    /// Held only while the queue changes: `commit`, `skip`, `stop` and the
    /// follower that plays songs on. Nothing slow ever holds it, which is what
    /// keeps Skip and Stop answering during a three-minute download.
    busy: Busy,
    /// Bumped by Stop, so a request still downloading knows it was cancelled.
    generations: Generations,
}

/// What adding a downloaded song to the queue did.
#[derive(Debug, PartialEq)]
enum Commit {
    /// Queued. `starting` says whether nothing was playing, in which case
    /// playback begins now rather than when the queue reaches it.
    Played { starting: bool },
    /// Stopped while this was downloading.
    Cancelled,
    /// Another request filled the queue while this one was downloading.
    QueueFull,
}

/// One guild's cancellation counter.
///
/// Named rather than spelled out at each use, because the type of the thing
/// being compared is not the point of any of these comparisons.
#[derive(Debug, Default)]
struct Cancellation(std::sync::atomic::AtomicU64);

impl Cancellation {
    fn now(&self) -> u64 {
        self.0.load(std::sync::atomic::Ordering::SeqCst)
    }

    fn cancel(&self) {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }
}

/// A queued song waiting to be judged: kept, or skipped too fast to trust.
///
/// Keyed by the audio file, so the judgement lands on the right song however
/// many are queued behind it. Bounded by the queue, which is bounded.
#[derive(Debug, Clone)]
struct Pending {
    /// What was asked for, as heard.
    said: String,
    /// What it turned out to mean.
    track: crate::library::Track,
    /// Whether a chooser button chose it, rather than a spoken request that
    /// resolved on its own.
    from_chooser: bool,
}

#[derive(Debug, Clone, Default)]
struct PendingLearn(Arc<Mutex<HashMap<std::path::PathBuf, Pending>>>);

impl PendingLearn {
    #[allow(dead_code)]
    fn set(&self, path: std::path::PathBuf, pending: Pending) {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(path, pending);
    }

    /// Takes the judgement for a song, so it is made once however the song
    /// ended.
    fn take(&self, path: &std::path::Path) -> Option<Pending> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(path)
    }
}

/// When the song playing now started, per guild.
#[derive(Debug, Clone, Default)]
struct StartedAt(Arc<Mutex<HashMap<GuildId, std::time::Instant>>>);

impl StartedAt {
    fn set(&self, guild_id: GuildId, at: std::time::Instant) {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(guild_id, at);
    }

    fn take(&self, guild_id: GuildId) -> Option<std::time::Instant> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&guild_id)
    }
}

/// A per-guild counter bumped whenever playback is cancelled.
///
/// A `stop` bumps it; a request still downloading compares against the number
/// it started with and drops its result rather than queueing a song into a
/// queue the listener just emptied. This is what makes Stop actually cancel a
/// pending request rather than merely clearing what is there now.
#[derive(Debug, Clone, Default)]
struct Generations(Arc<Mutex<HashMap<GuildId, Arc<Cancellation>>>>);

impl Generations {
    fn get(&self, guild_id: GuildId) -> Arc<Cancellation> {
        let mut all = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        Arc::clone(all.entry(guild_id).or_default())
    }

    /// Cancels everything in flight for this guild.
    fn bump(&self, guild_id: GuildId) {
        self.get(guild_id).cancel();
    }
}

impl Coordination {
    /// Claims the right to resolve one request for `guild_id`.
    ///
    /// The returned guard is held for the whole request and must be dropped at
    /// the end of it; `started_at` is the cancellation number to hand back to
    /// [`Self::commit`]. Read *after* the lock is taken, so a Stop that lands
    /// while this request waits its turn still counts against it.
    async fn begin(&self, guild_id: GuildId) -> (OwnedMutexGuard<()>, u64) {
        let order = self.order.get(guild_id);
        let guard = order.lock_owned().await;
        let started_at = self.generations.get(guild_id).now();
        (guard, started_at)
    }

    /// The lock a playback control takes.
    fn busy(&self, guild_id: GuildId) -> Arc<AsyncMutex<()>> {
        self.busy.get(guild_id)
    }

    /// Cancels every request in flight for `guild_id`.
    fn cancel(&self, guild_id: GuildId) {
        self.generations.bump(guild_id);
    }

    /// Adds an already-downloaded song to the queue.
    ///
    /// The only place a spoken request takes the playback lock, and it is taken
    /// after the search and the download rather than around them.
    async fn commit(
        &self,
        guild_id: GuildId,
        queue: &Queue,
        song: Queued,
        started_at: u64,
    ) -> Commit {
        let busy = self.busy.get(guild_id);
        let _held = busy.lock().await;
        if self.generations.get(guild_id).now() != started_at {
            tracing::info!(
                guild_id = %guild_id,
                title = %song.title,
                "stopped while this was downloading, so it is not being queued"
            );
            return Commit::Cancelled;
        }

        // An empty queue means nothing is playing, so this is the first song
        // and should start rather than wait.
        let starting = queue.is_idle();
        if !queue.push(song.clone()) {
            return Commit::QueueFull;
        }
        tracing::info!(
            guild_id = %guild_id,
            title = %song.title,
            artist = %song.artist,
            queued = queue.pending(),
            "queued a song"
        );
        Commit::Played { starting }
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

/// Where a guild's dashboard is, and what it is showing.
type Dashboard = (ChannelId, MessageId, Page);

/// The channel, message id and page of each guild's dashboard.
///
/// Only the ids are kept; the dashboard is rebuilt from the queue every time,
/// so a cached copy of the message would go stale. The page is kept because a
/// redraw must not yank the view away from someone who opened the queue to
/// read it -- a song ending while they are looking has to leave them looking.
#[derive(Debug, Clone, Default)]
struct Dashboards(Arc<Mutex<HashMap<GuildId, Dashboard>>>);

/// Which view a guild's dashboard is currently showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Page {
    NowPlaying,
    Queue,
}

impl Dashboards {
    /// The dashboard message for a guild, if it has one.
    fn get(&self, guild_id: GuildId) -> Option<MessageId> {
        self.lock().get(&guild_id).map(|(_, id, _)| *id)
    }

    /// The page a guild's dashboard is showing.
    fn page(&self, guild_id: GuildId) -> Page {
        self.lock()
            .get(&guild_id)
            .map(|(_, _, page)| *page)
            .unwrap_or(Page::NowPlaying)
    }

    /// Records where a guild's dashboard lives and what it is showing.
    fn set(&self, guild_id: GuildId, channel_id: ChannelId, message_id: MessageId, page: Page) {
        self.lock().insert(guild_id, (channel_id, message_id, page));
    }

    /// Forgets a guild's dashboard, so the next update posts a new one.
    fn forget(&self, guild_id: GuildId) {
        self.lock().remove(&guild_id);
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<GuildId, Dashboard>> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl Player {
    pub fn new(manager: Arc<Songbird>, catalog: Arc<resolve::Catalog>) -> Self {
        Self {
            manager,
            queues: Queues::default(),
            coordination: Coordination::default(),
            tracks: Tracks::default(),
            dashboards: Dashboards::default(),
            catalog,
            choices: Choices::default(),
            library: None,
            pending: PendingLearn::default(),
            started_at: StartedAt::default(),
            fetch: Arc::new(YtDlpFetch),
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
        rewrite: Option<&str>,
    ) -> anyhow::Result<()> {
        self.manager
            .get(guild_id)
            .context("bot is not in a voice channel")?;

        let heard = request.query.clone();
        match self
            .resolve_and_queue(config, guild_id, request, rewrite)
            .await
        {
            Ok(Outcome::Asked { tracks }) => {
                self.offer_songs(ctx, config, guild_id, request, &tracks)
                    .await
            }
            // Nothing was verified. Say so, and offer unverified results only if
            // the operator asked for them -- clearly labelled, and never played
            // on their own.
            Ok(Outcome::Unknown) => {
                let failure = Failure::new(
                    Problem::NoMatch,
                    anyhow::anyhow!("nothing in the catalog matched '{heard}'"),
                );
                self.report(ctx, config, guild_id, &heard, &failure).await;
                if config.unverified_chooser {
                    self.offer_unverified(ctx, config, guild_id, request).await;
                }
            }
            Ok(Outcome::Uploads(choice)) => {
                let asked = choice.requested.clone();
                self.offer_uploads(ctx, config, guild_id, &asked, &choice)
                    .await
            }
            Ok(Outcome::Played { starting: true }) => {
                self.start_next(ctx, config, guild_id).await;
            }
            // Either way the dashboard is redrawn: a played song needs naming,
            // and a cancelled one may have moved the queue along underneath it.
            Ok(_) => self.draw_dashboard(ctx, config, guild_id).await,
            Err(failure) => {
                self.report(ctx, config, guild_id, &heard, &failure).await;
                return Err(failure.error);
            }
        }
        Ok(())
    }

    /// Tells the listener their request did not become a song.
    ///
    /// A tone first, because the person who spoke is standing in a voice
    /// channel and may not be reading the text channel at all; then a message
    /// saying what was heard, which is the part they can act on.
    async fn report(
        &self,
        ctx: &SerenityContext,
        config: &Config,
        guild_id: GuildId,
        heard: &str,
        failure: &Failure,
    ) {
        tracing::warn!(
            guild_id = %guild_id,
            heard = %heard,
            problem = ?failure.problem,
            error = %format!("{failure:#}"),
            "the request did not become a song"
        );
        if let Err(e) = self.acknowledge(guild_id, Tone::Miss).await {
            tracing::warn!(guild_id = %guild_id, error = %format!("{e:#}"), "the miss tone failed");
        }

        let Some(channel_id) = resolve_alert_channel(ctx, config, guild_id) else {
            tracing::debug!(
                guild_id = %guild_id,
                "no alert channel, so there was nowhere to report the failure"
            );
            return;
        };
        let sent = channel_id
            .send_message(
                &ctx.http,
                dashboard::notice(&failure.message(heard)).into_message(),
            )
            .await;
        match sent {
            // The miss is worth reading once, not leaving in the channel as a
            // record, so it takes itself away after NOTICE_TTL.
            Ok(sent) => {
                let http = ctx.http.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(NOTICE_TTL).await;
                    if let Err(e) = http.delete_message(channel_id, sent.id, None).await {
                        tracing::debug!(
                            channel_id = %channel_id,
                            error = %format!("{e:#}"),
                            "could not take the notice away"
                        );
                    }
                });
            }
            Err(e) => {
                tracing::warn!(
                    guild_id = %guild_id,
                    error = %format!("{e:#}"),
                    "could not report the failed request"
                );
            }
        }
    }

    /// Turns a spoken request into a queued song, without touching Discord.
    ///
    /// The catalog lookup, the video search and the download all run here, and
    /// none of them holds the playback lock. Only
    /// [`Coordination::commit`] takes it, for the one step that changes the
    /// queue -- so a three-minute download cannot make Skip or Stop stop
    /// answering.
    async fn resolve_and_queue(
        &self,
        config: &Config,
        guild_id: GuildId,
        request: &PlayRequest,
        rewrite: Option<&str>,
    ) -> Result<Outcome, Failure> {
        let queue = self.queues.get(guild_id);
        // Checked before searching, so a full queue fails without a download.
        // `commit` checks again, since another request may have filled it.
        if queue.is_full() {
            return Err(Failure::new(
                Problem::QueueFull,
                anyhow::anyhow!("the queue is full, try again once a song finishes"),
            ));
        }

        // Claims the request ordering lock and reads the cancellation number to
        // hand back to `commit`. Read after the lock is taken, so a Stop that
        // lands while this request waits its turn still counts against it.
        let (_serialised, started_at) = self.coordination.begin(guild_id).await;

        let resolved = search::resolve_request(
            request,
            &self.catalog,
            rewrite,
            self.library.as_deref(),
            guild_id,
        )
        .await;
        if matches!(resolved, search::Resolved::Unknown { .. }) {
            return Ok(Outcome::Unknown);
        }
        if let search::Resolved::Ask(tracks) = resolved {
            return Ok(Outcome::Asked { tracks });
        }

        let label = resolved
            .as_track()
            .map(|track| track.label())
            .unwrap_or_default();
        let choice = search::find_video(
            &resolved,
            &request.query,
            &search::YtDlpSearch,
            config.search_candidates,
            config.match_threshold,
        )
        .await
        .map_err(|e| {
            Failure::new(
                Problem::NoMatch,
                e.context(format!("no upload for '{label}'")),
            )
        })?;

        let Some(best) = choice.best() else {
            return Err(Failure::new(
                Problem::NoMatch,
                anyhow::anyhow!("no playable upload of '{label}'"),
            ));
        };
        if !choice.confident {
            // The song is known; the upload is not. Offering the closest
            // uploads of *this* song is a much better question than asking which
            // song, and the answer is never wrong about which song it is.
            return Ok(Outcome::Uploads(choice));
        }

        // The request is being committed; record it for the learning that
        // happens when the song finishes (or is skipped).
        self.pending.set(
            PathBuf::from(best.candidate.url.clone()),
            Pending {
                said: request.query.clone(),
                track: crate::library::Track {
                    artist: resolved.as_track().unwrap().artist.clone(),
                    title: resolved.as_track().unwrap().title.clone(),
                    duration_secs: resolved.as_track().unwrap().duration_secs,
                },
                from_chooser: false,
            },
        );
        let song = self
            .fetch
            .fetch(guild_id, &best.candidate, &queue.in_use())
            .await
            .map_err(|e| Failure::new(Problem::FetchFailed, e))?;
        Ok(self.outcome(guild_id, &queue, song, started_at).await)
    }

    /// Adds a downloaded song and reports what became of it.
    async fn outcome(
        &self,
        guild_id: GuildId,
        queue: &Queue,
        song: Queued,
        started_at: u64,
    ) -> Outcome {
        match self
            .coordination
            .commit(guild_id, queue, song, started_at)
            .await
        {
            Commit::Played { starting } => Outcome::Played { starting },
            Commit::Cancelled => Outcome::Cancelled,
            Commit::QueueFull => {
                tracing::warn!(
                    guild_id = %guild_id,
                    "the queue filled up while this was downloading"
                );
                Outcome::Cancelled
            }
        }
    }

    /// Posts "which song did you mean" in the alert channel.
    ///
    /// The options are *songs* — "Stand By Me" by Ben E. King or by Oasis —
    /// because that is the question the listener was actually asking. Offering
    /// video titles instead answers a question about uploads they never asked.
    async fn offer_songs(
        &self,
        ctx: &SerenityContext,
        config: &Config,
        guild_id: GuildId,
        request: &PlayRequest,
        tracks: &[resolve::CatalogTrack],
    ) {
        let keys: Vec<String> = (0..tracks.len()).map(song_key).collect();
        let options: Vec<dashboard::ChoiceOption<'_>> = tracks
            .iter()
            .zip(&keys)
            .map(|(track, key)| dashboard::ChoiceOption {
                title: &track.title,
                channel: &track.artist,
                duration_secs: track.duration_secs,
                key,
            })
            .collect();
        let offered: Vec<(String, Offer)> = tracks
            .iter()
            .zip(&keys)
            .map(|(track, key)| {
                (
                    key.clone(),
                    Offer::Song {
                        track: track.clone(),
                        requested: request.query.clone(),
                    },
                )
            })
            .collect();
        self.choices.set(guild_id, offered);
        let question = request.query.clone();
        self.ask(ctx, config, guild_id, &question, None, &options)
            .await;
    }

    /// Offers raw YouTube results for a request nothing matched.
    ///
    /// These were never checked against a catalog, so they are labelled
    /// unverified and are never played on their own -- the user pressing one is
    /// the only thing that can start it. Better than silence when the catalog
    /// has never heard of the song, and worse than silence would be if any of
    /// them could start playing by themselves.
    async fn offer_unverified(
        &self,
        ctx: &SerenityContext,
        config: &Config,
        guild_id: GuildId,
        request: &PlayRequest,
    ) {
        match search::find_unverified(
            request,
            &search::YtDlpSearch,
            config.search_candidates,
            config.match_threshold,
        )
        .await
        {
            Ok(choice) => {
                let heard = request.query.clone();
                self.offer_uploads(ctx, config, guild_id, &heard, &choice)
                    .await
            }
            Err(e) => tracing::debug!(
                guild_id = %guild_id,
                error = %format!("{e:#}"),
                "nothing on YouTube either, so there was nothing to offer"
            ),
        }
    }

    /// Posts "which upload of this song did you mean".
    ///
    /// Used when the catalog named the song and the scorer was not sure which
    /// recording of it an upload actually is.
    async fn offer_uploads(
        &self,
        ctx: &SerenityContext,
        config: &Config,
        guild_id: GuildId,
        heard: &str,
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
                    key: video_id,
                })
            })
            .collect();
        let offered: Vec<(String, Offer)> = choice
            .top(CHOICE_COUNT)
            .iter()
            .filter_map(|scored| {
                Some((
                    scored.candidate.video_id()?.to_string(),
                    Offer::Video(scored.candidate.clone()),
                ))
            })
            .collect();
        self.choices.set(guild_id, offered);
        let resolved = choice.resolved.as_ref().map(|track| track.label());
        self.ask(ctx, config, guild_id, heard, resolved.as_deref(), &options)
            .await;
    }

    /// Posts the question, and takes it away again after [`CHOICE_TTL`].
    ///
    /// An answered question is deleted outright by the press that answered it,
    /// so a timer firing afterwards finds nothing left to remove and only has
    /// to strip the buttons.
    async fn ask(
        &self,
        ctx: &SerenityContext,
        config: &Config,
        guild_id: GuildId,
        heard: &str,
        resolved: Option<&str>,
        options: &[dashboard::ChoiceOption<'_>],
    ) {
        if options.is_empty() {
            let failure = Failure::new(
                Problem::NoMatch,
                anyhow::anyhow!("there was nothing to offer"),
            );
            self.report(ctx, config, guild_id, heard, &failure).await;
            return;
        }

        let Some(channel_id) = resolve_alert_channel(ctx, config, guild_id) else {
            let failure = Failure::new(
                Problem::NoMatch,
                anyhow::anyhow!("no alert channel, so there was nowhere to ask"),
            );
            self.report(ctx, config, guild_id, heard, &failure).await;
            return;
        };
        let question = format!("I heard \u{201c}{heard}\u{201d}");
        let message = dashboard::choice(&question, resolved, options);
        let Ok(sent) = channel_id.send_message(&ctx.http, message).await else {
            tracing::warn!(guild_id = %guild_id, "could not ask which song was meant");
            return;
        };

        let http = ctx.http.clone();
        tokio::spawn(async move {
            tokio::time::sleep(CHOICE_TTL).await;
            if http.delete_message(channel_id, sent.id, None).await.is_ok() {
                return;
            }
            // Nobody answered, so the question goes. If it cannot be deleted the
            // buttons still must go: a press from ten minutes ago cannot start
            // a download.
            if let Err(e) = http
                .edit_message(
                    channel_id,
                    sent.id,
                    &dashboard::expired(&question),
                    Vec::new(),
                )
                .await
            {
                tracing::debug!(
                    channel_id = %channel_id,
                    error = %format!("{e:#}"),
                    "could not take the chooser buttons away"
                );
            }
        });
    }

    /// Plays whatever a chooser button stands for.
    ///
    /// The key is looked up in the guild's choice store rather than acted on, so
    /// a crafted button cannot be pressed into playing something that was never
    /// offered, and a press is spent so a second one does nothing.
    pub async fn play_chosen(
        &self,
        ctx: &SerenityContext,
        config: &Config,
        guild_id: GuildId,
        key: &str,
    ) -> anyhow::Result<()> {
        let offer = self.choices.take(guild_id, key).with_context(
            || "that choice is no longer on offer; say the request again for new options",
        )?;

        // The user has already chosen the song by pressing this button, so the
        // best upload of it is taken even when the upload's own score is modest.
        // The hard part -- deciding which song it is -- was answered by the
        // catalog and by them.
        let candidate = match offer {
            Offer::Video(candidate) => candidate,
            Offer::Song { track, requested } => {
                let resolved = search::Resolved::Track {
                    track,
                    durations: Vec::new(),
                };
                let choice = search::find_video(
                    &resolved,
                    &requested,
                    &search::YtDlpSearch,
                    config.search_candidates,
                    // The user chose; the threshold is not what is in question
                    // any more.
                    0.0,
                )
                .await
                .with_context(|| "could not find that song on YouTube")?;
                choice
                    .best()
                    .map(|scored| scored.candidate.clone())
                    .context("YouTube returned nothing for that song")?
            }
        };

        // The download runs outside the playback lock for the same reason the
        // spoken path does: a press on "1" must not freeze the Skip button.
        let started_at = self.coordination.generations.get(guild_id).now();
        let song = self
            .fetch
            .fetch(guild_id, &candidate, &self.queues.get(guild_id).in_use())
            .await
            .context("could not download that song")?;
        let outcome = self
            .outcome(guild_id, &self.queues.get(guild_id), song, started_at)
            .await;

        if matches!(outcome, Outcome::Played { starting: true }) {
            self.start_next(ctx, config, guild_id).await;
        } else {
            self.draw_dashboard(ctx, config, guild_id).await;
        }
        Ok(())
    }

    /// Throws the guild's question away, for "None of these".
    ///
    /// Returns whether there was one, so the caller only says anything when the
    /// press did something.
    pub fn dismiss_choice(&self, guild_id: GuildId) -> bool {
        self.choices.clear(guild_id)
    }

    /// Skips the song playing, starting the next one queued.
    pub async fn skip(&self, ctx: &SerenityContext, config: &Config, guild_id: GuildId) {
        let lock = self.coordination.busy(guild_id);
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
            tracing::warn!(guild_id = %guild_id, error = %format!("{e:#}"), "failed to change playback state");
            return None;
        }
        queue.set_paused(paused);
        Some(paused)
    }

    /// Stops playback and clears the queue.
    ///
    /// The cancellation counter is bumped *before* the lock is taken, so a
    /// request still downloading will already see itself as cancelled by the
    /// time it tries to queue its song. Taking the lock first would leave a
    /// window in which a download commits and is then cleared anyway, which
    /// works but wastes the download.
    pub async fn stop(&self, ctx: &SerenityContext, config: &Config, guild_id: GuildId) {
        self.coordination.cancel(guild_id);
        let lock = self.coordination.busy(guild_id);
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
        // Whatever just stopped is the last word on whether the request that
        // queued it was a good one. Checked here rather than on a track event,
        // because this runs for a skip and for a song that finished on its own
        // alike, and songbird reports a skipped track as ended too.
        self.learn_from_stopped(guild_id, advanced.stopped.as_ref());
        if advanced.current.is_some() {
            self.started_at.set(guild_id, std::time::Instant::now());
        }
        if let Some(handle) = start_track(&self.manager, guild_id, &advanced).await {
            self.tracks.set(guild_id, handle.clone());
            self.follow_when_finished(ctx, config, guild_id, handle, advanced.generation);
        }
        self.draw_dashboard(ctx, config, guild_id).await;
    }

    /// Decides what a finished song says about the request that queued it.
    ///
    /// Kept for [`crate::library::KEPT_SECS`], the request is believed and
    /// remembered. Skipped sooner than that, it is feedback against the phrase
    /// that asked for it -- twice, and the phrase is forgotten. A song queued by
    /// pressing a chooser button was already believed when the button was
    /// pressed, so it is remembered either way.
    fn learn_from_stopped(&self, guild_id: GuildId, stopped: Option<&Queued>) {
        let (Some(stopped), Some(library)) = (stopped, self.library.as_deref()) else {
            return;
        };
        let Some(pending) = self.pending.take(&stopped.path) else {
            return;
        };

        let played_for = self
            .started_at
            .take(guild_id)
            .map_or(Duration::ZERO, |since| since.elapsed());
        if played_for >= Duration::from_secs(crate::library::KEPT_SECS) {
            tracing::info!(
                guild_id = %guild_id,
                said = %pending.said,
                track = %stopped.title,
                played_for_s = played_for.as_secs(),
                "remembering what that request meant"
            );
            library.remember(
                guild_id.get(),
                &pending.said,
                &pending.track,
                pending.from_chooser,
            );
        } else if pending.from_chooser {
            // The listener chose this by pressing it, so a quick skip is them
            // changing their mind rather than the bot having guessed wrong.
            library.remember(guild_id.get(), &pending.said, &pending.track, true);
        } else {
            library.quick_skip(&pending.said, &pending.track);
        }
        library.maybe_save();
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
            coordination: self.coordination.clone(),
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
                let lock = this.coordination.busy(guild_id);
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
        draw(&self.dashboards, &self.queues, ctx, config, guild_id, None).await;
    }

    /// Shows what is waiting in the queue instead of the now-playing view.
    pub async fn show_queue(&self, ctx: &SerenityContext, config: &Config, guild_id: GuildId) {
        draw(
            &self.dashboards,
            &self.queues,
            ctx,
            config,
            guild_id,
            Some(Page::Queue),
        )
        .await;
    }

    /// Goes back from the queue view to the now-playing one.
    pub async fn show_now_playing(
        &self,
        ctx: &SerenityContext,
        config: &Config,
        guild_id: GuildId,
    ) {
        draw(
            &self.dashboards,
            &self.queues,
            ctx,
            config,
            guild_id,
            Some(Page::NowPlaying),
        )
        .await;
    }

    /// Turns repeat-one on or off and redraws, so the button says which way it
    /// went. Returns the new setting.
    pub async fn toggle_repeat(
        &self,
        ctx: &SerenityContext,
        config: &Config,
        guild_id: GuildId,
    ) -> bool {
        let repeat = self.queues.get(guild_id).toggle_repeat();
        self.draw_dashboard(ctx, config, guild_id).await;
        repeat
    }
}

/// The parts of the player a songbird ending needs, owned so the callback can
/// outlive the call that set it up.
struct Follower {
    manager: Arc<Songbird>,
    queues: Queues,
    tracks: Tracks,
    dashboards: Dashboards,
    coordination: Coordination,
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
            None,
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
/// channel. The finished song's file is left on disk: `fetch::prune` removes
/// what is neither queued nor among the newest few, so deleting it here would
/// only undo the cache that makes a repeated request instant.
///
/// The call is locked with `await` rather than `blocking_lock`, which panics
/// when it is reached from inside a runtime thread.
async fn start_track(
    manager: &Songbird,
    guild_id: GuildId,
    advanced: &Advanced,
) -> Option<TrackHandle> {
    match (manager.get(guild_id), &advanced.current) {
        (Some(call), Some(song)) => {
            let mut handler = call.lock().await;
            handler.stop();
            tracing::info!(guild_id = %guild_id, title = %song.title, "playing");
            let path = song.path.clone();
            Some(handler.play_input(Input::from(FileInput::new(path))))
        }
        _ => None,
    }
}

/// Draws or updates a guild's dashboard message.
///
/// `page` is None to keep whichever page the dashboard is already on, which is
/// what every ordinary redraw wants: a song ending should not pull someone out
/// of the queue they opened.
async fn draw(
    dashboards: &Dashboards,
    queues: &Queues,
    ctx: &SerenityContext,
    config: &Config,
    guild_id: GuildId,
    page: Option<Page>,
) {
    let Some(channel) = resolve_alert_channel(ctx, config, guild_id) else {
        return;
    };
    let page = page.unwrap_or_else(|| dashboards.page(guild_id));
    let queue = queues.get(guild_id);
    let current = queue.current();
    let view = match page {
        Page::Queue => dashboard::queue_view(&queue.pending_songs()),
        Page::NowPlaying => match &current {
            Some(song) => {
                dashboard::dashboard(song, queue.is_paused(), queue.pending(), queue.is_repeat())
            }
            None => dashboard::idle_dashboard(),
        },
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
                tracing::warn!(guild_id = %guild_id, error = %format!("{e:#}"), "failed to update the dashboard");
                dashboards.forget(guild_id);
                if let Ok(sent) = channel.send_message(&ctx.http, view.into_message()).await {
                    dashboards.set(guild_id, channel, sent.id, page);
                }
            }
        }
        None => match channel.send_message(&ctx.http, view.into_message()).await {
            Ok(sent) => dashboards.set(guild_id, channel, sent.id, page),
            Err(e) => {
                tracing::warn!(guild_id = %guild_id, error = %format!("{e:#}"), "failed to post the dashboard")
            }
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

    /// Opening the queue is a choice, and a song ending underneath it must not
    /// close that choice: the redraw keeps whichever page is showing.
    #[test]
    fn a_redraw_keeps_the_page_the_dashboard_is_on() {
        let guild = GuildId::new(11);
        let dashboards = Dashboards::default();

        assert_eq!(dashboards.page(guild), Page::NowPlaying);

        dashboards.set(guild, ChannelId::new(1), MessageId::new(2), Page::Queue);
        assert_eq!(dashboards.page(guild), Page::Queue);

        // Forgetting the dashboard is what happens when a message is deleted;
        // the next post starts on now-playing rather than inheriting a page
        // whose message no longer exists.
        dashboards.forget(guild);
        assert_eq!(dashboards.page(guild), Page::NowPlaying);
    }

    /// The bug this commit exists for: `play` held the playback lock across the
    /// catalog lookup, the YouTube search *and the whole download*, and `skip()`
    /// and `stop()` take the same lock. A slow download therefore froze every
    /// playback control for that guild.
    ///
    /// `Songbird` cannot be constructed without a voice connection, so the
    /// coordination is exercised on its own. That is not a workaround: the
    /// whole bug is about which lock is held when, and none of it needs a
    /// connection to observe.
    #[tokio::test]
    async fn a_request_in_flight_does_not_hold_the_playback_lock() {
        let guild = GuildId::new(7);
        let coordination = Coordination::default();
        let queue = Queue::default();

        // What `resolve_and_queue` holds across the search and the download.
        let (_serialised, _started_at) = coordination.begin(guild).await;

        // What a Skip press needs, while that request is still in flight.
        let lock = coordination.busy(guild);
        let guard = tokio::time::timeout(Duration::from_millis(500), lock.lock_owned())
            .await
            .expect("a request in flight must not hold the playback lock");
        drop(guard);
        drop(_serialised);

        assert_eq!(queue.pending(), 0);
    }

    /// The same, from the fetcher's side: a download that takes the playback
    /// lock would block if the download were running inside it.
    #[tokio::test]
    async fn a_download_can_take_the_playback_lock_itself() {
        let guild = GuildId::new(7);
        let coordination = Coordination::default();
        let probe = LockProbe {
            coordination: coordination.clone(),
        };
        probe
            .fetch(guild, &candidate("https://youtu.be/x"), &[])
            .await
            .unwrap();
    }

    /// Stop really cancels: a request that was still downloading when Stop was
    /// pressed must not queue its song afterwards.
    #[tokio::test]
    async fn stop_cancels_a_request_that_is_still_downloading() {
        let guild = GuildId::new(7);
        let coordination = Coordination::default();
        let queue = Queue::default();

        let (_serialised, started_at) = coordination.begin(guild).await;
        // What `stop` does first, before it takes the playback lock.
        coordination.cancel(guild);

        let outcome = coordination
            .commit(guild, &queue, queued("Dracula"), started_at)
            .await;
        assert_eq!(
            outcome,
            Commit::Cancelled,
            "a song queued after a stop is a song nobody asked for"
        );
        assert!(queue.is_idle(), "the cancelled song must not be queued");
    }

    /// The other half: a request that was not cancelled still queues, and starts
    /// playing because nothing else is.
    #[tokio::test]
    async fn an_uncancelled_request_still_queues_and_starts() {
        let guild = GuildId::new(7);
        let coordination = Coordination::default();
        let queue = Queue::default();
        let (_serialised, started_at) = coordination.begin(guild).await;

        assert_eq!(
            coordination
                .commit(guild, &queue, queued("Dracula"), started_at)
                .await,
            Commit::Played { starting: true }
        );
        assert_eq!(queue.pending(), 1);
    }

    /// A song queued behind another does not start playing; it waits its turn.
    #[tokio::test]
    async fn a_second_song_waits_rather_than_starting() {
        let guild = GuildId::new(7);
        let coordination = Coordination::default();
        let queue = Queue::default();
        let (_serialised, started_at) = coordination.begin(guild).await;

        coordination
            .commit(guild, &queue, queued("First"), started_at)
            .await;
        assert_eq!(
            coordination
                .commit(guild, &queue, queued("Second"), started_at)
                .await,
            Commit::Played { starting: false }
        );
        assert_eq!(queue.pending(), 2);
    }

    /// Cancellation is per request, not per guild: a request that began after a
    /// Stop is unaffected by it.
    #[tokio::test]
    async fn a_request_started_after_a_stop_is_not_cancelled_by_it() {
        let guild = GuildId::new(7);
        let coordination = Coordination::default();
        let queue = Queue::default();
        coordination.cancel(guild);

        let (_serialised, started_at) = coordination.begin(guild).await;
        assert_eq!(
            coordination
                .commit(guild, &queue, queued("Dracula"), started_at)
                .await,
            Commit::Played { starting: true }
        );
    }

    /// Two requests for one guild do not overlap: the second waits its turn
    /// rather than both downloading and both starting a track.
    #[tokio::test]
    async fn two_requests_for_one_guild_are_served_in_order() {
        let guild = GuildId::new(7);
        let coordination = Coordination::default();
        let order = Arc::new(std::sync::Mutex::new(Vec::new()));

        let first = tokio::spawn({
            let coordination = coordination.clone();
            let order = Arc::clone(&order);
            async move {
                let (_guard, _at) = coordination.begin(guild).await;
                order.lock().unwrap().push("first");
                tokio::time::sleep(Duration::from_millis(30)).await;
            }
        });
        // Wait until the first request is definitely holding the lock.
        while order.lock().unwrap().is_empty() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let second = tokio::spawn({
            let coordination = coordination.clone();
            let order = Arc::clone(&order);
            async move {
                let (_guard, _at) = coordination.begin(guild).await;
                order.lock().unwrap().push("second");
            }
        });

        first.await.unwrap();
        second.await.unwrap();
        assert_eq!(*order.lock().unwrap(), ["first", "second"]);
    }

    /// Requests for different guilds never wait on each other.
    #[tokio::test]
    async fn requests_for_different_guilds_do_not_wait() {
        let coordination = Coordination::default();
        let (_one, _at) = coordination.begin(GuildId::new(1)).await;
        let other = coordination.busy(GuildId::new(2));
        tokio::time::timeout(Duration::from_millis(500), other.lock_owned())
            .await
            .expect("another guild's request must not block this one");
    }

    /// A full queue is reported rather than silently dropping the song.
    #[tokio::test]
    async fn a_queue_that_filled_up_while_downloading_is_reported() {
        let guild = GuildId::new(7);
        let coordination = Coordination::default();
        let queue = Queue::default();
        let (_serialised, started_at) = coordination.begin(guild).await;
        for i in 0..crate::queue::MAX_QUEUE {
            assert!(queue.push(queued(&format!("filler{i}"))));
        }

        assert_eq!(
            coordination
                .commit(guild, &queue, queued("Dracula"), started_at)
                .await,
            Commit::QueueFull
        );
    }

    /// Records whether the playback lock was free while it ran.
    struct LockProbe {
        coordination: Coordination,
    }

    #[async_trait]
    impl AudioFetcher for LockProbe {
        async fn fetch(
            &self,
            guild_id: GuildId,
            found: &search::Candidate,
            _in_use: &[std::path::PathBuf],
        ) -> anyhow::Result<Queued> {
            let lock = self.coordination.busy(guild_id);
            let guard = tokio::time::timeout(Duration::from_millis(500), lock.lock_owned())
                .await
                .expect("the playback lock was held while the audio downloaded");
            drop(guard);
            Ok(queued(found.title()))
        }
    }

    /// A song already in hand, as `commit` receives one.
    fn queued(title: &str) -> Queued {
        Queued {
            path: std::path::PathBuf::from(format!("/tmp/{title}.m4a")),
            title: title.to_string(),
            artist: "someone".to_string(),
            url: "https://youtu.be/only".to_string(),
            video_id: "only".to_string(),
        }
    }

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

    /// One offered option: the key its button carries, and what it would play.
    fn video_offer(url: &str) -> (String, Offer) {
        (url.to_string(), Offer::Video(candidate(url)))
    }

    fn song_offer(key: &str, artist: &str, title: &str) -> (String, Offer) {
        (
            key.to_string(),
            Offer::Song {
                track: resolve::CatalogTrack {
                    artist: artist.to_string(),
                    title: title.to_string(),
                    duration_secs: Some(191.0),
                    id: String::new(),
                    provider: "stub",
                    popularity: None,
                },
                requested: "stand by me".to_string(),
            },
        )
    }

    fn url_of(offer: Offer) -> String {
        match offer {
            Offer::Video(candidate) => candidate.url,
            Offer::Song { track, .. } => track.label(),
        }
    }

    /// A press has to find the option it stands for among the offered ones.
    #[test]
    fn a_press_finds_the_option_it_stands_for() {
        let choices = Choices::default();
        let guild = GuildId::new(7);
        choices.set(
            guild,
            vec![
                video_offer("https://youtu.be/first"),
                video_offer("https://youtu.be/second"),
            ],
        );
        let picked = choices
            .take(guild, "https://youtu.be/second")
            .expect("was offered");
        assert_eq!(url_of(picked), "https://youtu.be/second");
    }

    /// A catalog question offers songs, and the press carries a key that means
    /// nothing outside the store. That is the point: the button says "song 1",
    /// and the store says what song 1 is.
    #[test]
    fn a_song_press_resolves_to_the_song_it_named() {
        let choices = Choices::default();
        let guild = GuildId::new(7);
        choices.set(
            guild,
            vec![
                song_offer(&song_key(0), "Ben E. King", "Stand By Me"),
                song_offer(&song_key(1), "Oasis", "Stand By Me"),
            ],
        );
        let picked = choices.take(guild, &song_key(1)).expect("was offered");
        assert_eq!(url_of(picked), "Oasis — Stand By Me");
    }

    /// The buttons are spent by a press, so a second one must not replay the
    /// song.
    #[test]
    fn a_press_is_spent_once() {
        let choices = Choices::default();
        let guild = GuildId::new(7);
        choices.set(guild, vec![video_offer("https://youtu.be/only")]);
        assert!(choices.take(guild, "https://youtu.be/only").is_some());
        assert!(choices.take(guild, "https://youtu.be/only").is_none());
    }

    /// Something that was never offered must not be playable, or a crafted
    /// button could be pressed into playing anything at all.
    #[test]
    fn something_that_was_never_offered_is_refused() {
        let choices = Choices::default();
        let guild = GuildId::new(7);
        choices.set(guild, vec![video_offer("https://youtu.be/offered")]);
        assert!(choices.take(guild, "smuggled").is_none());
        assert!(choices.take(guild, &song_key(0)).is_none());
    }

    /// Asking again replaces the question, so a stale button cannot be pressed
    /// into playing something from a request that has since been forgotten.
    #[test]
    fn a_second_question_replaces_the_first() {
        let choices = Choices::default();
        let guild = GuildId::new(7);
        choices.set(guild, vec![video_offer("https://youtu.be/old")]);
        choices.set(guild, vec![video_offer("https://youtu.be/new")]);
        assert!(choices.take(guild, "https://youtu.be/old").is_none());
        assert!(choices.take(guild, "https://youtu.be/new").is_some());
    }

    /// The song keys restart at zero with every question, so a stale press from
    /// an older catalog question must not land on a song from a newer one.
    #[test]
    fn a_stale_song_key_cannot_reach_a_newer_question() {
        let choices = Choices::default();
        let guild = GuildId::new(7);
        choices.set(
            guild,
            vec![song_offer(&song_key(0), "Oasis", "Stand By Me")],
        );
        choices.set(guild, vec![song_offer(&song_key(0), "Tems", "Look at Me")]);
        let picked = choices.take(guild, &song_key(0)).expect("was offered");
        assert_eq!(
            url_of(picked),
            "Tems — Look at Me",
            "the key means whatever the current question says it means"
        );
    }

    /// One question per guild, so the map cannot grow with every ambiguous
    /// request, and a press in one guild cannot play in another.
    #[test]
    fn questions_are_kept_per_guild() {
        let choices = Choices::default();
        choices.set(GuildId::new(1), vec![video_offer("https://youtu.be/one")]);
        choices.set(GuildId::new(2), vec![video_offer("https://youtu.be/two")]);
        assert!(
            choices
                .take(GuildId::new(2), "https://youtu.be/one")
                .is_none()
        );
        assert_eq!(
            url_of(
                choices
                    .take(GuildId::new(1), "https://youtu.be/one")
                    .unwrap()
            ),
            "https://youtu.be/one"
        );
        // The untouched guild is unaffected by the other guild's press.
        assert_eq!(
            url_of(
                choices
                    .take(GuildId::new(2), "https://youtu.be/two")
                    .unwrap()
            ),
            "https://youtu.be/two"
        );
    }

    /// Once every button is spent the question is forgotten, rather than
    /// leaving an empty husk behind for the guild.
    #[test]
    fn a_fully_spent_question_is_forgotten() {
        let choices = Choices::default();
        let guild = GuildId::new(7);
        choices.set(guild, vec![video_offer("https://youtu.be/only")]);
        assert!(choices.take(guild, "https://youtu.be/only").is_some());
        assert!(choices.lock().is_empty());
    }

    /// "None of these" throws the question away, and says whether there was
    /// one so the caller only replies when the press did something.
    #[test]
    fn none_of_these_clears_the_question() {
        let choices = Choices::default();
        let guild = GuildId::new(7);
        assert!(!choices.clear(guild), "nothing was on offer");
        choices.set(guild, vec![video_offer("https://youtu.be/only")]);
        assert!(choices.clear(guild));
        assert!(!choices.clear(guild), "and not twice");
        assert!(choices.take(guild, "https://youtu.be/only").is_none());
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

    /// The miss tone has to be tellable from both acknowledgements, in either
    /// order. This is the whole point of adding it: a person who spoke and heard
    /// nothing has no way to tell the bot apart from one that did not hear them,
    /// so "I could not find that song" has to arrive as a sound and not only as
    /// a message in a text channel they may not be reading.
    #[test]
    fn the_miss_tone_is_tellable_from_both_acknowledgements() {
        let miss = frequency_of(&beep_wav(Tone::Miss.frequency()));
        assert!(
            (miss - Tone::Miss.frequency()).abs() < 20.0,
            "miss tone measured {miss} Hz"
        );
        for other in [Tone::Wake, Tone::Ready] {
            let their_frequency = other.frequency();
            let gap = (miss - their_frequency).abs();
            assert!(
                gap > 60.0,
                "{miss} Hz and {their_frequency} Hz are too close to tell apart"
            );
        }
        // And it is the odd one out, so it reads as "not an acknowledgement"
        // rather than as a third step in the same sequence.
        assert!(miss < Tone::Wake.frequency());
    }

    /// Every tone has to survive the same probe songbird runs on it, or a
    /// message that never arrives is worse than no message at all.
    #[test]
    fn every_tone_decodes() {
        use symphonia::core::formats::FormatOptions;
        use symphonia::core::io::MediaSourceStream;
        use symphonia::core::meta::MetadataOptions;
        use symphonia::core::probe::Hint;
        use symphonia::default::{get_codecs, get_probe};

        for tone in [Tone::Wake, Tone::Ready, Tone::Miss] {
            let wav = beep_wav(tone.frequency());
            let source =
                MediaSourceStream::new(Box::new(std::io::Cursor::new(wav)), Default::default());
            let mut probed = get_probe()
                .format(
                    &Hint::new(),
                    source,
                    &FormatOptions::default(),
                    &MetadataOptions::default(),
                )
                .expect("the probe must recognise the tone");
            let track = probed.format.default_track().expect("a track");
            let _decoder = get_codecs()
                .make(&track.codec_params, &Default::default())
                .expect("the pcm decoder must be registered");
            assert!(probed.format.next_packet().is_ok(), "{tone:?} must decode");
        }
    }

    /// The listener has to be told what was heard: the bot's transcription is
    /// frequently wrong, and echoing it back is what lets them say the right
    /// thing next time.
    #[test]
    fn every_failure_says_what_was_heard_and_what_to_do() {
        for (problem, expected) in [
            (
                Problem::NoMatch,
                "I heard “dracula by tame impala”, but couldn’t match it to a song. \
                 Try “play <title> by <artist>”.",
            ),
            (
                Problem::FetchFailed,
                "I found “dracula by tame impala” but couldn’t download it. \
                 Try again in a moment.",
            ),
            (
                Problem::QueueFull,
                "The queue is full, so “dracula by tame impala” wasn’t added. \
                 Skip or stop something first, then ask again.",
            ),
        ] {
            let failure = Failure::new(problem, anyhow::anyhow!("because"));
            assert_eq!(failure.message("dracula by tame impala"), expected);
        }
    }

    /// The three have to stay distinguishable as prose too, since a person
    /// reading them should not have to guess which went wrong.
    #[test]
    fn the_three_failures_read_differently() {
        let heard = "dracula by tame impala";
        let said = |problem| Failure::new(problem, anyhow::anyhow!("because")).message(heard);
        assert_ne!(said(Problem::NoMatch), said(Problem::FetchFailed));
        assert_ne!(said(Problem::NoMatch), said(Problem::QueueFull));
        assert_ne!(said(Problem::FetchFailed), said(Problem::QueueFull));
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
