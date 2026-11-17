use crate::config::Config;
use crate::dashboard;
use crate::fetch;
use crate::parser::PlayRequest;
use crate::queue::{Advanced, Queue, Queued, Queues, Source};
use crate::resolve;
use crate::search;
use crate::timing::Timing;
use anyhow::Context;
use serenity::async_trait;
use serenity::model::channel::ChannelType;
use serenity::model::id::{ChannelId, GuildId, MessageId, UserId};
use serenity::prelude::Context as SerenityContext;
use songbird::Songbird;
use songbird::events::{Event, EventContext, EventHandler, TrackEvent};
use songbird::input::{File as FileInput, HttpRequest, Input};
use songbird::tracks::TrackHandle;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;
use tokio::sync::Mutex as AsyncMutex;
use tokio::sync::OwnedMutexGuard;
use tokio::sync::mpsc;
use tokio::task::AbortHandle;

/// Length of the acknowledgement tone.
const BEEP_DURATION: Duration = Duration::from_millis(150);
/// Songbird mixes at 48 kHz stereo, so the tone needs no resampling.
const BEEP_SAMPLE_RATE: u32 = 48_000;
const BEEP_CHANNELS: u16 = 2;

/// Which acknowledgement tone to play; a tone apart so speakers can tell the stages apart by ear.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    /// The wake word was heard. The first model is done listening.
    Wake,
    /// The larger model is loaded and ready for the request.
    Ready,
    /// Nothing became of the request: no match, no result, or a failed fetch.
    Miss,
}

impl Tone {
    /// A5, the next one up, and A4: the miss drops an octave so it can't pass for an ack.
    pub fn frequency(self) -> f32 {
        match self {
            Self::Wake => 880.0,
            Self::Ready => 987.77,
            Self::Miss => 440.0,
        }
    }
}

/// What became of a resolved request, returned so the Discord side stays separate from song choice.
#[derive(Debug)]
pub enum Outcome {
    /// Queued; `starting` means nothing was playing, so it begins now.
    Played { starting: bool },
    /// Nothing was convincing enough; offer these, after the album when one was named.
    Asked {
        tracks: Vec<resolve::CatalogTrack>,
        album: Option<resolve::CatalogAlbum>,
    },
    /// The words named an album and no song: every track of it is queued.
    Album(resolve::CatalogAlbum),
    /// Uploads of one known song, none certain enough: a choice between recordings, not songs.
    Uploads(search::Choice),
    /// Nothing matched at all.
    Unknown,
    /// Playback was stopped while this was still downloading.
    Cancelled,
}

/// Why a request could not become a song; each gets a tone and a message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Problem {
    /// Nothing in the catalog or on YouTube was convincing enough.
    NoMatch,
    /// The song was found but its audio could not be downloaded.
    FetchFailed,
    /// The queue had no room.
    QueueFull,
    /// "Album" was said, and no album answered to the rest.
    NoAlbum,
}

/// A failure as its own type, so message wording can change without changing which one is shown.
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

    /// The listener's message, echoing what was heard so they know what to say instead.
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
            Problem::NoAlbum => format!(
                "I heard “{heard}”, but couldn’t find an album by that name. \
                 Try “play the album <title> by <artist>”."
            ),
        }
    }
}

/// Plays queued music and keeps the text channel dashboard in step.
pub struct Player {
    pub manager: Arc<Songbird>,
    /// The per-guild play queues, shared with the interaction handler.
    pub queues: Queues,
    /// Serialises requests and playback controls per guild.
    coordination: Coordination,
    /// The track playing per guild, needed to pause and resume it.
    tracks: Tracks,
    /// Each guild's dashboard, so a new song edits it instead of posting another.
    dashboards: Dashboards,
    /// The timer per guild that moves the dashboard's progress bar along.
    refreshes: Refreshes,
    /// Resolves a spoken request to a real song before YouTube is searched.
    catalog: Arc<resolve::Catalog>,
    /// The outstanding chooser per guild.
    asks: Asks,
    /// Who has the music lowered while they talk to the bot.
    ducks: Ducks,
    /// What the bot remembers, or `None` without a place to store it.
    pub library: Option<Arc<crate::library::Library>>,
    /// Queued songs waiting to be judged: kept, or skipped too fast to trust?
    pending: PendingLearn,
    /// When the song now playing started, per guild.
    started_at: StartedAt,
    /// Downloads the audio for a chosen video.
    fetch: Arc<dyn AudioFetcher>,
    /// Cache of tracks offered in autocomplete choices.
    autocomplete_cache: AutocompleteCache,
}

/// Turns a search result into a queued song; a trait so tests control when the download happens.
#[serenity::async_trait]
pub trait AudioFetcher: Send + Sync {
    /// Downloads `found`; `in_use` files must not be pruned, or songbird plays silence.
    async fn fetch(
        &self,
        guild_id: GuildId,
        found: &search::Candidate,
        in_use: &[std::path::PathBuf],
        timing: &Timing,
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
        timing: &Timing,
    ) -> anyhow::Result<Queued> {
        // No id means no cache or thumbnail, but the audio still plays.
        let video_id = found.video_id().unwrap_or_default().to_string();
        // Fetched before queueing, so a failure leaves the queue alone.
        let source = fetch::fetch(
            &video_id,
            &found.url,
            &fetch::cache_dir(guild_id.get()),
            in_use,
            timing,
        )
        .await
        .with_context(|| format!("could not fetch '{}'", found.title()))?;

        Ok(Queued {
            source,
            title: found.title().to_string(),
            artist: found.channel().to_string(),
            url: found.url.clone(),
            video_id,
            duration_secs: found.duration.filter(|d| *d > 0.0),
            // Callers override this for non-spoken requests.
            origin: crate::queue::Origin::Voice,
            // Set by the caller when the song memory chose it.
            remembered: None,
        })
    }
}

/// How long a miss notice stays up: long enough to read, short enough not to pile up.
const NOTICE_TTL: Duration = Duration::from_secs(30);

/// How long a chooser stays up before it is cleared.
const ASK_TTL: Duration = Duration::from_secs(30);

/// What one chooser button plays: a catalog song (upload found on press) or a specific upload.
#[derive(Debug, Clone)]
enum Offer {
    /// A specific upload, from a search nothing catalog-backed produced.
    Video(search::Candidate),
    /// A catalog song the user already picked, so its best upload is taken even at a modest score.
    Song {
        track: resolve::CatalogTrack,
        requested: String,
    },
    /// A whole album, EP or single: pressing it queues every track.
    Album(resolve::CatalogAlbum),
}

/// How a chosen song was asked for: shown on the dashboard, and for a pick, the words to remember.
#[derive(Debug, Clone, Copy)]
struct Via<'a> {
    said: Option<&'a str>,
    origin: crate::queue::Origin,
}

/// Who may answer: a typed search is the typer's, a spoken one the channel's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Owner {
    /// Whoever typed `/play`.
    User(UserId),
    /// Anyone in the voice channel, which the caller has already checked.
    Voice,
}

/// An outstanding question; pages are windows onto one list, so paging never re-searches.
#[derive(Debug, Clone)]
struct Ask {
    /// The words typed or heard, echoed and used for the picked song's upload search.
    query: String,
    /// The cleaned words a pick is remembered by; `None` for e.g. a link.
    said: Option<String>,
    /// Everything on offer, in rank order.
    offers: Vec<Offer>,
    /// Which window is on screen.
    page: usize,
    owner: Owner,
    /// Where the chooser was posted, so paging edits it; `None` until sent.
    posted: Option<(ChannelId, MessageId)>,
}

impl Ask {
    /// The offers on the page currently shown.
    fn page(&self) -> &[Offer] {
        let start = (self.page * dashboard::ASK_PAGE).min(self.offers.len());
        let end = (start + dashboard::ASK_PAGE).min(self.offers.len());
        &self.offers[start..end]
    }

    /// Whether there is another page behind this one.
    fn has_next(&self) -> bool {
        (self.page + 1) * dashboard::ASK_PAGE < self.offers.len()
    }

    /// The chooser as it would be drawn.
    fn view(&self) -> dashboard::View {
        let options = self
            .page()
            .iter()
            .map(|offer| offer.option())
            .collect::<Vec<_>>();
        dashboard::ask(&self.query, &options, self.page, self.offers.len())
    }
}

impl Offer {
    /// A song shows its artist, an upload its channel: the only things telling uploads apart.
    fn option(&self) -> dashboard::ChoiceOption<'_> {
        match self {
            Offer::Video(candidate) => dashboard::ChoiceOption {
                title: candidate.title(),
                channel: candidate.channel(),
                duration_secs: candidate.duration,
                plays: candidate.view_count,
                release: None,
                year: None,
            },
            Offer::Song { track, .. } => dashboard::ChoiceOption {
                title: &track.title,
                channel: &track.artist,
                duration_secs: track.duration_secs,
                plays: track.popularity.map(u64::from),
                release: None,
                year: None,
            },
            Offer::Album(album) => dashboard::ChoiceOption {
                title: &album.title,
                channel: &album.artist,
                duration_secs: None,
                plays: None,
                release: Some(album.kind.label()),
                year: album.year,
            },
        }
    }
}

/// The outstanding question per guild; asking again replaces it.
#[derive(Debug, Clone, Default)]
struct Asks(Arc<Mutex<HashMap<GuildId, Ask>>>);

impl Asks {
    fn set(&self, guild_id: GuildId, ask: Ask) {
        self.lock().insert(guild_id, ask);
    }

    /// Puts the new question in place and hands back the one it replaced.
    fn replace(&self, guild_id: GuildId, ask: Ask) -> Option<Ask> {
        self.lock().insert(guild_id, ask)
    }

    fn clear(&self, guild_id: GuildId) -> bool {
        self.lock().remove(&guild_id).is_some()
    }

    /// Clears only if still the timer's message, so an old timer cannot clear a newer chooser.
    fn clear_if(&self, guild_id: GuildId, channel_id: ChannelId, message_id: MessageId) -> bool {
        let mut asks = self.lock();
        if asks
            .get(&guild_id)
            .is_some_and(|ask| ask.posted == Some((channel_id, message_id)))
        {
            asks.remove(&guild_id);
            true
        } else {
            false
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<GuildId, Ask>> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Voice streams lowering the music per guild; it stays low while any of them is talking.
#[derive(Debug, Clone, Default)]
struct Ducks(Arc<Mutex<HashMap<GuildId, std::collections::HashSet<u32>>>>);

impl Ducks {
    /// Records `ssrc` lowering or releasing; returns whether the music should now be low.
    fn set(&self, guild_id: GuildId, ssrc: u32, on: bool) -> bool {
        let mut all = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        let holders = all.entry(guild_id).or_default();
        if on {
            holders.insert(ssrc);
        } else {
            holders.remove(&ssrc);
        }
        !holders.is_empty()
    }

    fn clear(&self, guild_id: GuildId) {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&guild_id);
    }

    fn is_ducked(&self, guild_id: GuildId) -> bool {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&guild_id)
            .is_some_and(|holders| !holders.is_empty())
    }
}

/// Music volume while somebody is talking to the bot.
pub const DUCK_VOLUME: f32 = 0.2;

#[derive(Debug, Clone, Default)]
struct AutocompleteCache(Arc<Mutex<HashMap<String, resolve::CatalogTrack>>>);

impl AutocompleteCache {
    fn insert(&self, video_id: String, track: resolve::CatalogTrack) {
        let mut map = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        if map.len() > 500 {
            map.clear();
        }
        map.insert(video_id, track);
    }

    fn get(&self, video_id: &str) -> Option<resolve::CatalogTrack> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(video_id)
            .cloned()
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

/// Per-guild coordination of requests and playback controls; testable without a voice connection.
#[derive(Debug, Clone, Default)]
struct Coordination {
    /// Held for a whole request; FIFO, so concurrent requests play in the order spoken.
    order: Busy,
    /// Held only while the queue changes, never across anything slow, so Skip and Stop stay live.
    busy: Busy,
    /// Bumped by Stop, so a request still downloading knows it was cancelled.
    generations: Generations,
}

/// What `/play` did with a playlist link, for its reply.
#[derive(Debug, Clone, PartialEq)]
pub struct PlaylistQueued {
    pub title: String,
    /// The first track, already queued: "artist — title".
    pub first: String,
    /// Whether the first track started playing, rather than waiting its turn.
    pub starting: bool,
    /// Tracks being queued, the first included.
    pub queued: usize,
    /// Tracks that did not fit in the queue.
    pub left_out: usize,
    /// Tracks that could not be played: private, deleted, or failed to load.
    pub unavailable: usize,
}

impl PlaylistQueued {
    /// What was queued, as the `/play` reply or a spoken album notice.
    pub fn summary(&self) -> String {
        let others = self.queued - 1;
        let first = dashboard::markdown_escape(&self.first);
        let title = dashboard::markdown_escape(&self.title);
        let mut text = if self.starting {
            format!("Playing **{first}** from **{title}**")
        } else {
            format!("Queued **{first}** from **{title}**")
        };
        if others > 0 {
            text.push_str(&format!(
                ", and adding the other {others} song{} behind it.",
                if others == 1 { "" } else { "s" }
            ));
        } else {
            text.push('.');
        }
        if self.left_out > 0 {
            text.push_str(&format!(
                "\n{} more didn't fit: the queue holds {} songs.",
                self.left_out,
                crate::queue::MAX_QUEUE
            ));
        }
        if self.unavailable > 0 {
            text.push_str(&format!(
                "\n{} couldn't be played (private, deleted or unavailable).",
                self.unavailable
            ));
        }
        text
    }
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
#[derive(Debug, Clone)]
struct Pending {
    /// What was asked for, as heard.
    said: String,
    /// What it turned out to mean.
    track: crate::library::Track,
}

/// What a chooser pick is remembered as: the catalog song, else the upload, with the played video.
fn picked_track(
    song: Option<&resolve::CatalogTrack>,
    candidate: &search::Candidate,
) -> Option<crate::library::Track> {
    let video_id = candidate.video_id().map(str::to_string);
    Some(match song {
        Some(song) => crate::library::Track {
            artist: song.artist.clone(),
            title: song.title.clone(),
            duration_secs: song.duration_secs,
            video_id,
        },
        None => crate::library::Track {
            artist: candidate.channel.clone().unwrap_or_default(),
            title: candidate.title.clone().filter(|t| !t.trim().is_empty())?,
            duration_secs: candidate.duration,
            video_id,
        },
    })
}

/// The words a typed pick is remembered by, cleaned like speech; links are never remembered.
fn remembered_as(typed: &str) -> Option<String> {
    let is_link = typed.split_whitespace().any(|word| {
        let word = word.to_ascii_lowercase();
        word.contains("://") || word.starts_with("www.")
    }) || crate::playlist::video_id(typed).is_some()
        || crate::playlist::playlist_url(typed).is_some();
    if is_link {
        return None;
    }
    crate::parser::typed_request(typed).map(|request| request.query)
}

/// Moves `song` to the front of `tracks`, adding it if missing.
fn put_first(tracks: &mut Vec<resolve::CatalogTrack>, song: resolve::CatalogTrack) {
    let same = |track: &resolve::CatalogTrack| {
        crate::matcher::normalize(&track.artist) == crate::matcher::normalize(&song.artist)
            && crate::matcher::normalize(&track.title) == crate::matcher::normalize(&song.title)
    };
    let song = match tracks.iter().position(same) {
        // The remembered video wins: it is the one that was picked and played.
        Some(at) => {
            let found = tracks.remove(at);
            resolve::CatalogTrack {
                video_id: song.video_id.or(found.video_id),
                ..found
            }
        }
        None => song,
    };
    tracks.insert(0, song);
}

/// Typed or remembered words, read as a spoken request would be.
fn request_for(words: &str) -> PlayRequest {
    PlayRequest {
        query: words.to_string(),
        title: None,
        artist: None,
        splits: crate::parser::readings(words)
            .into_iter()
            .filter_map(|(title, artist)| Some((title?, artist?)))
            .collect(),
    }
}

/// The songs to offer again without `wrong`, unless it is the only one.
fn without_wrong(
    tracks: Vec<resolve::CatalogTrack>,
    wrong: Option<&crate::library::Track>,
) -> Vec<resolve::CatalogTrack> {
    let Some(wrong) = wrong else {
        return tracks;
    };
    let is_wrong = |track: &resolve::CatalogTrack| {
        crate::matcher::normalize(&track.artist) == crate::matcher::normalize(&wrong.artist)
            && crate::matcher::normalize(&track.title) == crate::matcher::normalize(&wrong.title)
    };
    if tracks.iter().all(is_wrong) {
        return tracks;
    }
    tracks
        .into_iter()
        .filter(|track| !is_wrong(track))
        .collect()
}

/// Songs waiting to be judged, keyed by video URL since the audio file is not a stable name.
#[derive(Debug, Clone, Default)]
struct PendingLearn(Arc<Mutex<HashMap<String, Pending>>>);

impl PendingLearn {
    fn set(&self, url: String, pending: Pending) {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(url, pending);
    }

    /// Takes a song's judgement, so it is made once however the song ended.
    fn take(&self, url: &str) -> Option<Pending> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(url)
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

/// A per-guild counter bumped by `stop`, so a request still downloading drops its result.
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
    /// Claims one request for `guild_id`; hold the guard and pass the number to [`Self::commit`].
    /// Read after locking, so a Stop while waiting still counts.
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

    /// Queues an already-downloaded song; the only place a request takes the playback lock.
    async fn commit(
        &self,
        guild_id: GuildId,
        queue: &Queue,
        song: Queued,
        started_at: u64,
        timing: &Timing,
    ) -> Commit {
        timing
            .stage(
                "commit",
                self.commit_locked(guild_id, queue, song, started_at),
            )
            .await
    }

    /// Body of [`Self::commit`], split so early returns cannot skip the timing wrapper.
    async fn commit_locked(
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

        // An empty queue means nothing is playing, so this one starts.
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

/// The playing track per guild, kept because pause and resume need its handle.
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

/// Each guild's dashboard ids and page; the page is kept so a redraw doesn't leave the queue view.
#[derive(Debug, Clone, Default)]
struct Dashboards(Arc<Mutex<HashMap<GuildId, Dashboard>>>);

/// Which view a guild's dashboard is currently showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Page {
    NowPlaying,
    /// The queue, with the id of the waiting song picked for editing, if any.
    Queue {
        selected: Option<u64>,
    },
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

/// How often a playing dashboard redraws its bar; each redraw is a rate-limited edit.
const REFRESH_EVERY: Duration = Duration::from_secs(15);

/// The progress bar timer per guild; each redraw replaces it.
#[derive(Debug, Clone, Default)]
struct Refreshes(Arc<Mutex<HashMap<GuildId, AbortHandle>>>);

impl Refreshes {
    /// Stops the guild's timer, if it has one.
    fn stop(&self, guild_id: GuildId) {
        if let Some(timer) = self.lock().remove(&guild_id) {
            timer.abort();
        }
    }

    /// Makes `timer` the guild's timer, stopping any it had before.
    fn set(&self, guild_id: GuildId, timer: AbortHandle) {
        if let Some(old) = self.lock().insert(guild_id, timer) {
            old.abort();
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<GuildId, AbortHandle>> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Whether a fresh dashboard has a moving bar: now-playing, unpaused, with a bar.
fn worth_refreshing(page: Page, has_bar: bool, paused: bool) -> bool {
    page == Page::NowPlaying && has_bar && !paused
}

/// What drawing a dashboard needs, owned so a refresh timer can outlive the caller.
#[derive(Clone)]
struct Board {
    dashboards: Dashboards,
    queues: Queues,
    tracks: Tracks,
    refreshes: Refreshes,
}

impl Player {
    /// The parts of the player drawing a dashboard needs.
    fn board(&self) -> Board {
        Board {
            dashboards: self.dashboards.clone(),
            queues: self.queues.clone(),
            tracks: self.tracks.clone(),
            refreshes: self.refreshes.clone(),
        }
    }

    pub fn new(manager: Arc<Songbird>, catalog: Arc<resolve::Catalog>) -> Self {
        Self {
            manager,
            queues: Queues::default(),
            coordination: Coordination::default(),
            tracks: Tracks::default(),
            dashboards: Dashboards::default(),
            refreshes: Refreshes::default(),
            catalog,
            asks: Asks::default(),
            ducks: Ducks::default(),
            library: None,
            pending: PendingLearn::default(),
            started_at: StartedAt::default(),
            fetch: Arc::new(YtDlpFetch),
            autocomplete_cache: AutocompleteCache::default(),
        }
    }

    /// Restores full volume whoever lowered it, e.g. when listening is locked.
    pub fn release_duck(&self, guild_id: GuildId) {
        self.ducks.clear(guild_id);
        if let Some(handle) = self.tracks.get(guild_id) {
            let _ = handle.set_volume(1.0);
        }
    }

    /// Lowers the music while `ssrc` talks, so an open mic does not feed lyrics to the model.
    pub fn duck(&self, guild_id: GuildId, ssrc: u32, on: bool) {
        let low = self.ducks.set(guild_id, ssrc, on);
        if let Some(handle) = self.tracks.get(guild_id) {
            let _ = handle.set_volume(if low { DUCK_VOLUME } else { 1.0 });
        }
    }

    /// Plays an ack tone over the music and waits for it, so back-to-back tones don't merge.
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

    /// Resolves the request against the catalog and queues it, or asks when nothing is convincing.
    pub async fn play(
        self: &Arc<Self>,
        ctx: &SerenityContext,
        config: &Config,
        guild_id: GuildId,
        request: &PlayRequest,
        rewrite: Option<&str>,
        timing: &Timing,
    ) -> anyhow::Result<()> {
        self.manager
            .get(guild_id)
            .context("bot is not in a voice channel")?;

        // "Play the album …" says what it wants, so only albums are searched.
        if let Some(named) = crate::parser::album_request(request) {
            self.play_named_album(ctx, config, guild_id, &named, timing)
                .await;
            return Ok(());
        }

        let heard = request.query.clone();
        match self
            .resolve_and_queue(config, guild_id, request, rewrite, timing)
            .await
        {
            Ok(Outcome::Asked { tracks, album }) => {
                self.offer_songs(ctx, config, guild_id, request, &tracks, album)
                    .await
            }
            Ok(Outcome::Album(album)) => {
                self.play_album(ctx, config, guild_id, &request.query, &album)
                    .await
            }
            // Nothing verified: offer unverified results only if the operator opted in.
            Ok(Outcome::Unknown) => {
                let failure = Failure::new(
                    Problem::NoMatch,
                    anyhow::anyhow!("nothing in the catalog matched '{heard}'"),
                );
                self.report(ctx, config, guild_id, &heard, &failure).await;
                if config.unverified_chooser {
                    self.offer_unverified(ctx, config, guild_id, request, timing)
                        .await;
                }
            }
            Ok(Outcome::Uploads(choice)) => {
                let asked = choice.requested.clone();
                self.offer_uploads(ctx, config, guild_id, &asked, &choice)
                    .await
            }
            Ok(Outcome::Played { starting: true }) => {
                self.start_next(ctx, config, guild_id, timing).await;
            }
            // Redraw either way: a cancelled request may have moved the queue.
            Ok(_) => self.draw_dashboard(ctx, config, guild_id).await,
            Err(failure) => {
                self.report(ctx, config, guild_id, &heard, &failure).await;
                return Err(failure.error);
            }
        }
        Ok(())
    }

    /// Tells the listener the request failed: a tone first, then a message with what was heard.
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

        self.post_notice(
            ctx,
            config,
            guild_id,
            dashboard::notice(&failure.message(heard)),
        )
        .await;
    }

    /// Posts a short notice that deletes itself after [`NOTICE_TTL`].
    async fn post_notice(
        &self,
        ctx: &SerenityContext,
        config: &Config,
        guild_id: GuildId,
        view: dashboard::View,
    ) {
        let Some(channel_id) = resolve_alert_channel(ctx, config, guild_id) else {
            tracing::debug!(
                guild_id = %guild_id,
                "no alert channel, so there was nowhere to post the notice"
            );
            return;
        };
        match channel_id
            .send_message(&ctx.http, view.into_message())
            .await
        {
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
                    "could not post the notice"
                );
            }
        }
    }

    /// Plays the album a request named, or asks which when several match.
    async fn play_named_album(
        self: &Arc<Self>,
        ctx: &SerenityContext,
        config: &Config,
        guild_id: GuildId,
        request: &PlayRequest,
        timing: &Timing,
    ) {
        let heard = request.query.clone();
        let albums = timing
            .stage(
                "catalog.albums",
                self.catalog.albums(&crate::album::query(request)),
            )
            .await;
        match crate::album::find(&heard, &albums, true) {
            crate::album::Found::One(album) => {
                self.play_album(ctx, config, guild_id, &heard, &album).await
            }
            crate::album::Found::Several(albums) => {
                let offers = albums.into_iter().map(Offer::Album).collect();
                self.offer_choices(ctx, config, guild_id, &heard, offers)
                    .await;
            }
            crate::album::Found::Nothing => {
                let failure = Failure::new(
                    Problem::NoAlbum,
                    anyhow::anyhow!("no album in the catalog matched '{heard}'"),
                );
                self.report(ctx, config, guild_id, &heard, &failure).await;
            }
        }
    }

    /// Queues every track of `album` like a pasted album link: first now, the rest in background.
    async fn play_album(
        self: &Arc<Self>,
        ctx: &SerenityContext,
        config: &Config,
        guild_id: GuildId,
        heard: &str,
        album: &resolve::CatalogAlbum,
    ) {
        tracing::info!(guild_id = %guild_id, heard = %heard, album = %album.label(), "playing an album");
        if self.queues.get(guild_id).is_full() {
            let failure = Failure::new(
                Problem::QueueFull,
                anyhow::anyhow!("the queue is full, so the album was not added"),
            );
            self.report(ctx, config, guild_id, &album.label(), &failure)
                .await;
            return;
        }
        match self
            .play_playlist(ctx, config, guild_id, &album.url())
            .await
        {
            Ok(mut queued) => {
                // The listing calls it "Album - 17"; the catalog knows better.
                queued.title = album.label();
                self.post_notice(ctx, config, guild_id, dashboard::info(&queued.summary()))
                    .await;
            }
            Err(e) => {
                let failure = Failure::new(Problem::FetchFailed, e);
                self.report(ctx, config, guild_id, &album.label(), &failure)
                    .await;
            }
        }
    }

    /// Turns a spoken request into a queued song, without touching Discord.
    /// Only [`Coordination::commit`] takes the playback lock, so downloads never block Skip.
    async fn resolve_and_queue(
        &self,
        config: &Config,
        guild_id: GuildId,
        request: &PlayRequest,
        rewrite: Option<&str>,
        timing: &Timing,
    ) -> Result<Outcome, Failure> {
        let queue = self.queues.get(guild_id);
        // Fail fast before downloading; `commit` checks again.
        if queue.is_full() {
            return Err(Failure::new(
                Problem::QueueFull,
                anyhow::anyhow!("the queue is full, try again once a song finishes"),
            ));
        }

        let (_serialised, started_at) = self.coordination.begin(guild_id).await;

        // A taught phrase plays what it was taught.
        let remembered = self
            .library
            .as_deref()
            .and_then(|library| search::remembered(library, &request.query));
        let recalled = remembered.is_some();
        let resolved = match remembered {
            Some(resolved) => resolved,
            None => {
                // Song and album looked up together, so albums add no wait; see `crate::album`.
                let album_query = crate::album::query(request);
                let (resolved, albums) = tokio::join!(
                    search::resolve_request(
                        request,
                        &self.catalog,
                        rewrite,
                        self.library.as_deref(),
                        guild_id,
                        timing,
                    ),
                    timing.stage("catalog.albums", self.catalog.albums(&album_query)),
                );
                let album = crate::album::find(&request.query, &albums, false);
                match crate::album::settle(&request.query, &resolved, album) {
                    crate::album::Settle::Song => resolved,
                    crate::album::Settle::Album(album) => return Ok(Outcome::Album(album)),
                    crate::album::Settle::Ask { album, songs } => {
                        return Ok(Outcome::Asked {
                            tracks: songs,
                            album: Some(album),
                        });
                    }
                }
            }
        };
        if matches!(resolved, search::Resolved::Unknown) {
            return Ok(Outcome::Unknown);
        }
        if let search::Resolved::Ask(tracks) = resolved {
            return Ok(Outcome::Asked {
                tracks,
                album: None,
            });
        }

        let Some(track) = resolved.as_track() else {
            return Ok(Outcome::Unknown);
        };
        let label = track.label();
        let choice = search::find_video(
            &resolved,
            &request.query,
            &search::YtDlpSearch,
            config.search_candidates,
            config.match_threshold,
            timing,
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
            // Song known, upload uncertain: ask which upload, not which song.
            return Ok(Outcome::Uploads(choice));
        }

        let mut song = self
            .fetch
            .fetch(guild_id, &best.candidate, &queue.in_use(), timing)
            .await
            .map_err(|e| Failure::new(Problem::FetchFailed, e))?;
        song.remembered = recalled.then(|| request.query.clone());

        // Recorded only after a successful fetch, for judging when the song stops.
        self.pending.set(
            best.candidate.url.clone(),
            Pending {
                said: request.query.clone(),
                track: crate::library::Track {
                    artist: track.artist.clone(),
                    title: track.title.clone(),
                    duration_secs: track.duration_secs,
                    // The played upload, so the phrase can go straight to it next time.
                    video_id: best.candidate.video_id().map(str::to_string),
                },
            },
        );
        let outcome = self
            .outcome(guild_id, &queue, song, started_at, timing)
            .await;
        if matches!(outcome, Outcome::Cancelled) {
            // Never queued, so drop it before it misjudges a later play of this video.
            self.pending.take(&best.candidate.url);
        }
        Ok(outcome)
    }

    /// Adds a downloaded song and reports what became of it.
    async fn outcome(
        &self,
        guild_id: GuildId,
        queue: &Queue,
        song: Queued,
        started_at: u64,
        timing: &Timing,
    ) -> Outcome {
        match self
            .coordination
            .commit(guild_id, queue, song, started_at, timing)
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

    /// Posts "which song did you mean", offering songs rather than uploads.
    async fn offer_songs(
        &self,
        ctx: &SerenityContext,
        config: &Config,
        guild_id: GuildId,
        request: &PlayRequest,
        tracks: &[resolve::CatalogTrack],
        album: Option<resolve::CatalogAlbum>,
    ) {
        let offers = album
            .into_iter()
            .map(Offer::Album)
            .chain(tracks.iter().map(|track| Offer::Song {
                track: track.clone(),
                requested: request.query.clone(),
            }))
            .collect();
        self.offer_choices(ctx, config, guild_id, &request.query, offers)
            .await;
    }

    /// Offers unverified YouTube results; only a button press can play one.
    async fn offer_unverified(
        &self,
        ctx: &SerenityContext,
        config: &Config,
        guild_id: GuildId,
        request: &PlayRequest,
        timing: &Timing,
    ) {
        match search::find_unverified(
            request,
            &search::YtDlpSearch,
            config.search_candidates,
            config.match_threshold,
            timing,
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

    /// Offers uploads of a catalog song when none matched well enough to auto-play.
    async fn offer_uploads(
        &self,
        ctx: &SerenityContext,
        config: &Config,
        guild_id: GuildId,
        heard: &str,
        choice: &search::Choice,
    ) {
        let offers: Vec<Offer> = choice
            .top(dashboard::ASK_PAGE)
            .iter()
            // No video id: nothing to play or key the button on.
            .filter(|scored| scored.candidate.video_id().is_some())
            .map(|scored| Offer::Video(scored.candidate.clone()))
            .collect();
        self.offer_choices(ctx, config, guild_id, heard, offers)
            .await;
    }

    /// Skips the song playing, starting the next one queued.
    pub async fn skip(&self, ctx: &SerenityContext, config: &Config, guild_id: GuildId) {
        let lock = self.coordination.busy(guild_id);
        let _guard = lock.lock().await;
        // Not part of a request, so nothing is timed.
        self.start_next(ctx, config, guild_id, &Timing::default())
            .await;
    }

    /// Sets the asked-for pause state (not a toggle, so double presses are safe); `None` if idle.
    pub async fn set_paused(&self, guild_id: GuildId, paused: bool) -> Option<bool> {
        let handle = self.tracks.get(guild_id)?;
        let queue = self.queues.get(guild_id);
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
    /// Cancels before locking, so an in-flight download drops itself instead of committing.
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

    /// Wrong song: forgets the phrase, skips, and asks again for the same words.
    /// Returns false when the song wasn't chosen by the memory (a stale press).
    pub async fn wrong_song(
        &self,
        ctx: &SerenityContext,
        config: &Config,
        guild_id: GuildId,
    ) -> bool {
        let (said, forgotten) = {
            // Under Skip's lock, so a song ending now cannot make this skip the next one.
            let lock = self.coordination.busy(guild_id);
            let _guard = lock.lock().await;
            let Some(song) = self.queues.get(guild_id).current() else {
                return false;
            };
            let Some(said) = song.remembered.clone() else {
                return false;
            };
            let forgotten = self.library.as_deref().and_then(|library| {
                let forgotten = library.forget(&said);
                library.save();
                forgotten
            });
            // Already judged by the listener; the skip must not count as quick too.
            self.pending.take(&song.url);
            tracing::info!(guild_id = %guild_id, said = %said, "wrong song; asking again");
            self.start_next(ctx, config, guild_id, &Timing::default())
                .await;
            (said, forgotten)
        };
        self.ask_again(ctx, config, guild_id, &said, forgotten.as_ref())
            .await;
        true
    }

    /// Re-asks "Which one?" for words the memory got wrong, leaving out the wrong song if possible.
    async fn ask_again(
        &self,
        ctx: &SerenityContext,
        config: &Config,
        guild_id: GuildId,
        said: &str,
        wrong: Option<&crate::library::Track>,
    ) {
        let request = request_for(said);
        let timing = Timing::default();
        let tracks: Vec<resolve::CatalogTrack> = timing
            .stage(
                "catalog.hits",
                self.catalog.hits(&resolve::Target::from_request(&request)),
            )
            .await
            .iter()
            .map(|hit| hit.to_track())
            .collect();
        let tracks = without_wrong(tracks, wrong);
        if tracks.is_empty() {
            if config.unverified_chooser {
                self.offer_unverified(ctx, config, guild_id, &request, &timing)
                    .await;
            } else {
                let failure = Failure::new(
                    Problem::NoMatch,
                    anyhow::anyhow!("nothing in the catalog matched '{said}'"),
                );
                self.report(ctx, config, guild_id, said, &failure).await;
            }
            return;
        }
        self.offer_songs(ctx, config, guild_id, &request, &tracks, None)
            .await;
    }

    /// Stops the current song and plays the next one queued.
    async fn start_next(
        &self,
        ctx: &SerenityContext,
        config: &Config,
        guild_id: GuildId,
        timing: &Timing,
    ) {
        let queue = self.queues.get(guild_id);
        let advanced = queue.advance();
        // Here rather than on a track event: covers skips and natural ends alike.
        self.learn_from_stopped(guild_id, advanced.stopped.as_ref());
        if advanced.current.is_some() {
            self.started_at.set(guild_id, std::time::Instant::now());
        }
        let (handle, advanced, skipped) =
            start_playable(&self.manager, guild_id, &queue, advanced, timing).await;
        if let Some(handle) = handle {
            if self.ducks.is_ducked(guild_id) {
                let _ = handle.set_volume(DUCK_VOLUME);
            }
            self.tracks.set(guild_id, handle.clone());
            self.follow_when_finished(ctx, config, guild_id, handle, advanced.generation);
        }
        self.draw_dashboard(ctx, config, guild_id).await;
        for title in &skipped {
            let failure = Failure::new(
                Problem::FetchFailed,
                anyhow::anyhow!("'{title}' could not start, so it was skipped"),
            );
            self.report(ctx, config, guild_id, title, &failure).await;
        }
    }

    /// Lets a stopped song count against a remembered phrase (skipped within
    /// [`crate::library::KEPT_SECS`] twice and it is forgotten); only picks teach the memory.
    fn learn_from_stopped(&self, guild_id: GuildId, stopped: Option<&Queued>) {
        let (Some(stopped), Some(library)) = (stopped, self.library.as_deref()) else {
            return;
        };
        let Some(pending) = self.pending.take(&stopped.url) else {
            return;
        };

        let played_for = self
            .started_at
            .take(guild_id)
            .map_or(Duration::ZERO, |since| since.elapsed());
        if played_for < Duration::from_secs(crate::library::KEPT_SECS) {
            library.quick_skip(&pending.said, &pending.track);
            library.maybe_save();
        }
    }

    /// Remembers that `said` meant the picked song; saved at once so a restart can't lose it.
    fn remember_pick(
        &self,
        guild_id: GuildId,
        said: &str,
        song: Option<&resolve::CatalogTrack>,
        candidate: &search::Candidate,
    ) {
        let Some(library) = self.library.as_deref() else {
            return;
        };
        let Some(track) = picked_track(song, candidate) else {
            return;
        };
        tracing::info!(
            guild_id = %guild_id,
            said = %said,
            track = %track.label(),
            "remembering the song picked for that request"
        );
        library.remember(guild_id.get(), said, &track, true);
        library.save();
    }

    /// Starts the next song when this one ends; the generation check makes a skip's ending a no-op.
    fn follow_when_finished(
        &self,
        ctx: &SerenityContext,
        config: &Config,
        guild_id: GuildId,
        handle: TrackHandle,
        generation: u64,
    ) {
        let (tx, mut finished) = mpsc::unbounded_channel();
        // Runs on a songbird thread, so it only wakes the task below.
        let _ = handle.add_event(Event::Track(TrackEvent::End), Ended(tx));

        let this = Follower {
            manager: Arc::clone(&self.manager),
            queues: self.queues.clone(),
            tracks: self.tracks.clone(),
            board: self.board(),
            coordination: self.coordination.clone(),
            ctx: ctx.clone(),
            config: config.clone(),
            guild_id,
        };
        tokio::spawn(async move {
            let mut generation = generation;
            while finished.recv().await.is_some() {
                // Same lock as the commands, so an ending and a skip don't interleave.
                let lock = this.coordination.busy(guild_id);
                let _held = lock.lock().await;

                // A skip may have advanced already; the check makes a stale ending harmless.
                let Some(advanced) = this.queues.get(guild_id).advance_if_current(generation)
                else {
                    return;
                };
                let (handle, advanced, _skipped) = start_playable(
                    &this.manager,
                    guild_id,
                    &this.queues.get(guild_id),
                    advanced,
                    &Timing::default(),
                )
                .await;
                // Track the new generation, or the next song looks superseded and the queue stops.
                generation = advanced.generation;

                match handle {
                    Some(handle) => {
                        this.tracks.set(guild_id, handle.clone());
                        // Watch the new song so the queue keeps moving.
                        let (tx, rx) = mpsc::unbounded_channel();
                        let _ = handle.add_event(Event::Track(TrackEvent::End), Ended(tx));
                        finished = rx;
                        // Redraw per advance, or the dashboard names a song that already ended.
                        this.draw_dashboard().await;
                    }
                    None => break,
                }
            }
            this.draw_dashboard().await;
        });
    }

    /// Adopts a pressed dashboard, since ids are lost on restart.
    pub fn adopt_dashboard(&self, guild_id: GuildId, channel_id: ChannelId, message_id: MessageId) {
        let page = self.dashboards.page(guild_id);
        self.dashboards.set(guild_id, channel_id, message_id, page);
    }

    /// Creates or updates the guild's dashboard; skipped silently without a channel.
    pub async fn draw_dashboard(&self, ctx: &SerenityContext, config: &Config, guild_id: GuildId) {
        draw(&self.board(), ctx, config, guild_id, None).await;
    }

    /// Shows what is waiting in the queue instead of the now-playing view.
    pub async fn show_queue(&self, ctx: &SerenityContext, config: &Config, guild_id: GuildId) {
        self.pick_in_queue(ctx, config, guild_id, None).await;
    }

    /// Redraws the queue view with `selected` picked for editing.
    pub async fn pick_in_queue(
        &self,
        ctx: &SerenityContext,
        config: &Config,
        guild_id: GuildId,
        selected: Option<u64>,
    ) {
        draw(
            &self.board(),
            ctx,
            config,
            guild_id,
            Some(Page::Queue { selected }),
        )
        .await;
    }

    /// Removes or moves waiting song `id` and redraws; by id, so a stale pick touches nothing else.
    pub async fn edit_queue(
        &self,
        ctx: &SerenityContext,
        config: &Config,
        guild_id: GuildId,
        edit: dashboard::Edit,
        id: u64,
    ) {
        let queue = self.queues.get(guild_id);
        let selected = match edit {
            dashboard::Edit::Remove => {
                match queue.remove(id) {
                    Some(song) => {
                        tracing::info!(guild_id = %guild_id, title = %song.title, "removed from the queue")
                    }
                    None => {
                        tracing::info!(guild_id = %guild_id, id, "the song to remove is no longer waiting")
                    }
                }
                None
            }
            dashboard::Edit::Shift(shift) => match queue.shift(id, shift) {
                Some(position) => {
                    tracing::info!(guild_id = %guild_id, id, ?shift, position, "moved in the queue");
                    // Kept picked, so pressing up again moves it again.
                    Some(id)
                }
                None => {
                    tracing::info!(guild_id = %guild_id, id, "the song to move is no longer waiting");
                    None
                }
            },
        };
        self.pick_in_queue(ctx, config, guild_id, selected).await;
    }

    /// Goes back from the queue view to the now-playing one.
    pub async fn show_now_playing(
        &self,
        ctx: &SerenityContext,
        config: &Config,
        guild_id: GuildId,
    ) {
        draw(&self.board(), ctx, config, guild_id, Some(Page::NowPlaying)).await;
    }

    /// Toggles repeat-one and redraws; returns the new setting.
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

    /// Answers a `/play` search with the first page of catalog songs to choose from.
    pub async fn search_and_ask(
        &self,
        ctx: &SerenityContext,
        config: &Config,
        guild_id: GuildId,
        query: &str,
        asker: UserId,
    ) -> anyhow::Result<usize> {
        // Its own request with its own summary line.
        let timing = Timing::default();
        let searched = self
            .search_and_ask_inner(ctx, config, guild_id, query, asker, &timing)
            .await;
        timing.log("/play search");
        searched
    }

    async fn search_and_ask_inner(
        &self,
        ctx: &SerenityContext,
        config: &Config,
        guild_id: GuildId,
        query: &str,
        asker: UserId,
        timing: &Timing,
    ) -> anyhow::Result<usize> {
        let request = request_for(query);
        let mut tracks: Vec<resolve::CatalogTrack> = timing
            .stage(
                "catalog.hits",
                self.catalog.hits(&resolve::Target::from_request(&request)),
            )
            .await
            .iter()
            .map(|hit| hit.to_track())
            .collect();

        // A remembered pick goes first, but still as a question: `/play` asked to choose.
        let said = remembered_as(query);
        let recalled = said
            .as_deref()
            .zip(self.library.as_deref())
            .and_then(|(said, library)| library.recall(said));
        if let Some(track) = recalled {
            tracing::info!(guild_id = %guild_id, said = ?said, track = %track.label(), "recalled from the song library");
            put_first(&mut tracks, track.to_catalog_track());
        }

        let total = tracks.len();
        if total == 0 {
            self.asks.clear(guild_id);
            anyhow::bail!("the catalog has nothing for \"{query}\"");
        }

        let ask = Ask {
            offers: tracks
                .into_iter()
                .map(|track| Offer::Song {
                    track,
                    requested: query.to_string(),
                })
                .collect(),
            query: query.to_string(),
            said,
            page: 0,
            owner: Owner::User(asker),
            posted: None,
        };
        self.post_ask(ctx, config, guild_id, ask).await?;
        tracing::info!(guild_id = %guild_id, query = %query, found = total, "asked the catalog");
        Ok(total)
    }

    /// Posts a chooser, clears any previous question, and arms the expiry timer.
    async fn post_ask(
        &self,
        ctx: &SerenityContext,
        config: &Config,
        guild_id: GuildId,
        mut ask: Ask,
    ) -> anyhow::Result<()> {
        let Some(channel_id) = resolve_alert_channel(ctx, config, guild_id) else {
            anyhow::bail!("no alert channel, so there was nowhere to post the search");
        };
        let sent = channel_id
            .send_message(&ctx.http, ask.view().into_message())
            .await
            .map_err(|e| anyhow::anyhow!("could not post the search: {e}"))?;
        ask.posted = Some((channel_id, sent.id));
        let previous = self.asks.replace(guild_id, ask);
        if let Some((old_channel, old_message)) = previous.and_then(|a| a.posted) {
            let http = ctx.http.clone();
            tokio::spawn(async move {
                if let Err(e) = http.delete_message(old_channel, old_message, None).await {
                    tracing::debug!(
                        channel_id = %old_channel,
                        error = %format!("{e:#}"),
                        "could not clear the previous question"
                    );
                }
            });
        }
        self.expire_ask(ctx, guild_id, channel_id, sent.id);
        Ok(())
    }

    fn expire_ask(
        &self,
        ctx: &SerenityContext,
        guild_id: GuildId,
        channel_id: ChannelId,
        message_id: MessageId,
    ) {
        let asks = self.asks.clone();
        let http = ctx.http.clone();
        tokio::spawn(async move {
            tokio::time::sleep(ASK_TTL).await;
            if !asks.clear_if(guild_id, channel_id, message_id) {
                return;
            }
            if let Err(e) = http.delete_message(channel_id, message_id, None).await {
                tracing::debug!(
                    channel_id = %channel_id,
                    error = %format!("{e:#}"),
                    "could not clear the expired question"
                );
            }
        });
    }

    /// Turns to the next page of a `/play` search; returns whether it moved.
    pub async fn next_page(&self, ctx: &SerenityContext, guild_id: GuildId) -> bool {
        // Bumped on a copy and stored only if the redraw landed, so racing presses stay consistent.
        let Some(mut ask) = self.ask_for(guild_id) else {
            return false;
        };
        if !ask.has_next() {
            return false;
        }
        ask.page += 1;
        if self.redraw_ask(ctx, guild_id, ask.view()).await {
            self.asks.set(guild_id, ask);
            true
        } else {
            false
        }
    }

    /// Plays the song a `/play` search put at `index`.
    pub async fn play_asked(
        self: &Arc<Self>,
        ctx: &SerenityContext,
        config: &Config,
        guild_id: GuildId,
        index: usize,
    ) -> anyhow::Result<()> {
        let ask = self
            .ask_for(guild_id)
            .context("that search is no longer on offer; run /play again")?;
        let offer = ask
            .offers
            .get(index)
            .cloned()
            .with_context(|| "that song is not in the search")?;
        self.asks.clear(guild_id);
        if let Offer::Album(album) = &offer {
            self.play_album(ctx, config, guild_id, &ask.query, album)
                .await;
            return Ok(());
        }
        // Remembered for the words that asked, typed or spoken.
        let said = ask.said.as_deref();
        let origin = match ask.owner {
            Owner::Voice => crate::queue::Origin::Voice,
            Owner::User(_) => crate::queue::Origin::Slash,
        };
        self.play_offer(ctx, config, guild_id, offer, Via { said, origin })
            .await
    }

    /// Whether the guild still holds a question to answer.
    pub fn has_ask(&self, guild_id: GuildId) -> bool {
        self.ask_for(guild_id).is_some()
    }

    /// Whether the question came from speech (channel answers) rather than `/play` (typer answers).
    pub fn ask_is_spoken(&self, guild_id: GuildId) -> bool {
        self.ask_for(guild_id)
            .is_some_and(|ask| matches!(ask.owner, Owner::Voice))
    }

    /// Offers `offers` for a spoken request; same chooser as `/play`, owned by the channel.
    async fn offer_choices(
        &self,
        ctx: &SerenityContext,
        config: &Config,
        guild_id: GuildId,
        query: &str,
        offers: Vec<Offer>,
    ) {
        if offers.is_empty() {
            return;
        }
        let ask = Ask {
            query: query.to_string(),
            said: Some(query.to_string()),
            offers,
            page: 0,
            owner: Owner::Voice,
            posted: None,
        };
        if let Err(e) = self.post_ask(ctx, config, guild_id, ask).await {
            tracing::warn!(guild_id = %guild_id, error = %format!("{e:#}"), "could not post the question");
        }
    }

    /// Plays whatever an offered choice stands for, typed or spoken.
    async fn play_offer(
        &self,
        ctx: &SerenityContext,
        config: &Config,
        guild_id: GuildId,
        offer: Offer,
        via: Via<'_>,
    ) -> anyhow::Result<()> {
        // Pressing a button is a request of its own, with its own summary line.
        let timing = Timing::default();
        let _ = self
            .play_offer_inner(ctx, config, guild_id, offer, via, &timing)
            .await?;
        timing.log("/play offer");
        Ok(())
    }

    async fn play_offer_inner(
        &self,
        ctx: &SerenityContext,
        config: &Config,
        guild_id: GuildId,
        offer: Offer,
        via: Via<'_>,
        timing: &Timing,
    ) -> anyhow::Result<(String, bool)> {
        // The user already chose the song, so take its best upload even at a modest score.
        let (candidate, song_track) = match offer {
            Offer::Video(candidate) => (candidate, None),
            Offer::Song { track, requested } => {
                let resolved = search::Resolved::Track {
                    durations: track.duration_secs.into_iter().collect(),
                    track: track.clone(),
                };
                let choice = search::find_video(
                    &resolved,
                    &requested,
                    &search::YtDlpSearch,
                    config.search_candidates,
                    0.0,
                    timing,
                )
                .await
                .with_context(|| "could not find that song on YouTube")?;
                let candidate = choice
                    .best()
                    .map(|scored| scored.candidate.clone())
                    .context("YouTube returned nothing for that song")?;
                (candidate, Some(track))
            }
            Offer::Album(album) => {
                anyhow::bail!("{} is an album, which plays as one", album.label())
            }
        };

        // Outside the playback lock so Skip never freezes; ordered so only one starts playback.
        let (_serialised, started_at) = self.coordination.begin(guild_id).await;
        let mut song = self
            .fetch
            .fetch(
                guild_id,
                &candidate,
                &self.queues.get(guild_id).in_use(),
                timing,
            )
            .await
            .context("could not download that song")?;
        song.origin = via.origin;
        if let Some(said) = via.said {
            self.remember_pick(guild_id, said, song_track.as_ref(), &candidate);
        }
        let label = format!("{} — {}", song.artist, song.title);
        let outcome = self
            .outcome(
                guild_id,
                &self.queues.get(guild_id),
                song,
                started_at,
                timing,
            )
            .await;

        let starting = matches!(outcome, Outcome::Played { starting: true });
        if starting {
            self.start_next(ctx, config, guild_id, timing).await;
        } else {
            self.draw_dashboard(ctx, config, guild_id).await;
        }
        Ok((label, starting))
    }

    /// Autocomplete candidates for a slash command query.
    pub async fn autocomplete(&self, query: &str) -> Vec<resolve::CatalogTrack> {
        let request = request_for(query);
        let target = resolve::Target::from_request(&request);
        let hits = self.catalog.hits(&target).await;
        let mut tracks = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for hit in hits {
            let track = hit.to_track();
            if let Some(video_id) = &track.video_id
                && seen.insert(video_id.clone())
            {
                self.autocomplete_cache
                    .insert(video_id.clone(), track.clone());
                tracks.push(track);
            }
        }
        tracks
    }

    /// Queues every track of a playlist or album for `/play <link>`.
    ///
    /// The first is queued before returning, the rest in the background until Stop or a full queue.
    pub async fn play_playlist(
        self: &Arc<Self>,
        ctx: &SerenityContext,
        config: &Config,
        guild_id: GuildId,
        url: &str,
    ) -> anyhow::Result<PlaylistQueued> {
        self.manager
            .get(guild_id)
            .context("bot is not in a voice channel")?;
        let playlist = crate::playlist::list(url).await?;
        anyhow::ensure!(
            !playlist.tracks.is_empty(),
            "{} has no tracks that can be played",
            playlist.title
        );
        let listed = playlist.tracks.len();
        let room = crate::playlist::fitting(
            listed,
            self.queues.get(guild_id).pending(),
            crate::queue::MAX_QUEUE,
        );
        anyhow::ensure!(
            room > 0,
            "the queue is full ({} songs); skip some or wait",
            crate::queue::MAX_QUEUE
        );
        self.asks.clear(guild_id);
        // Anything that bumps this, a Stop above all, ends the background queuing.
        let generation = self.coordination.generations.get(guild_id).now();

        let mut tracks = playlist.tracks.into_iter().take(room);
        let mut unplayable = 0;
        let mut first = None;
        for track in tracks.by_ref() {
            match self.play_track(ctx, config, guild_id, &track).await {
                Ok(played) => {
                    first = Some(played);
                    break;
                }
                Err(e) => {
                    unplayable += 1;
                    tracing::warn!(guild_id = %guild_id, track = %track.title, error = %format!("{e:#}"), "a playlist track could not be played");
                }
            }
        }
        let (first, starting) = first.context("none of its tracks could be played")?;
        let rest: Vec<crate::playlist::Track> = tracks.collect();
        let queued = 1 + rest.len();
        tracing::info!(guild_id = %guild_id, playlist = %playlist.title, listed, queued, "queuing a playlist");

        let this = Arc::clone(self);
        let ctx = ctx.clone();
        let config = config.clone();
        tokio::spawn(async move {
            this.queue_rest(&ctx, &config, guild_id, rest, generation)
                .await;
        });

        Ok(PlaylistQueued {
            title: playlist.title,
            first,
            starting,
            queued,
            left_out: listed - room,
            unavailable: playlist.unavailable + unplayable,
        })
    }

    /// Queues the tracks after the first, one at a time and in order.
    async fn queue_rest(
        &self,
        ctx: &SerenityContext,
        config: &Config,
        guild_id: GuildId,
        rest: Vec<crate::playlist::Track>,
        generation: u64,
    ) {
        let total = rest.len();
        let mut added = 0;
        for track in rest {
            if self.coordination.generations.get(guild_id).now() != generation {
                tracing::info!(guild_id = %guild_id, added, total, "playback was stopped; the rest of the playlist is dropped");
                return;
            }
            if self.manager.get(guild_id).is_none() {
                tracing::info!(guild_id = %guild_id, added, total, "left the voice channel; the rest of the playlist is dropped");
                return;
            }
            if self.queues.get(guild_id).is_full() {
                tracing::info!(guild_id = %guild_id, added, total, "the queue filled up; the rest of the playlist is dropped");
                return;
            }
            match self.play_track(ctx, config, guild_id, &track).await {
                Ok(_) => added += 1,
                Err(e) => {
                    tracing::warn!(guild_id = %guild_id, track = %track.title, error = %format!("{e:#}"), "a playlist track could not be played; going on to the next")
                }
            }
        }
        tracing::info!(guild_id = %guild_id, added, total, "the rest of the playlist is queued");
    }

    /// Downloads one playlist track and queues it; the track is the song, so nothing is searched.
    async fn play_track(
        &self,
        ctx: &SerenityContext,
        config: &Config,
        guild_id: GuildId,
        track: &crate::playlist::Track,
    ) -> anyhow::Result<(String, bool)> {
        let candidate = search::Candidate {
            title: Some(track.title.clone()),
            url: track.url(),
            view_count: None,
            duration: track.duration_secs,
            channel: track.channel.clone(),
            id: Some(track.video_id.clone()),
        };
        let timing = Timing::default();
        let played = self
            .play_offer_inner(
                ctx,
                config,
                guild_id,
                Offer::Video(candidate),
                Via {
                    said: None,
                    origin: crate::queue::Origin::Playlist,
                },
                &timing,
            )
            .await;
        timing.log("/play playlist track");
        played
    }

    /// Plays or queues a specific YouTube track chosen via autocomplete or direct URL.
    pub async fn play_direct(
        &self,
        ctx: &SerenityContext,
        config: &Config,
        guild_id: GuildId,
        query: &str,
        video_id: &str,
    ) -> anyhow::Result<(String, bool)> {
        self.manager
            .get(guild_id)
            .context("bot is not in a voice channel")?;

        self.asks.clear(guild_id);

        let offer = if let Some(track) = self.autocomplete_cache.get(video_id) {
            Offer::Song {
                track,
                requested: query.to_string(),
            }
        } else {
            let candidate = search::Candidate {
                title: None,
                url: format!("https://www.youtube.com/watch?v={video_id}"),
                view_count: None,
                duration: None,
                channel: None,
                id: Some(video_id.to_string()),
            };
            Offer::Video(candidate)
        };

        let timing = Timing::default();
        let res = self
            .play_offer_inner(
                ctx,
                config,
                guild_id,
                offer,
                Via {
                    said: None,
                    origin: crate::queue::Origin::Slash,
                },
                &timing,
            )
            .await;
        timing.log("/play direct");
        res
    }

    /// Throws a `/play` search away, for "None of these".
    pub fn dismiss_ask(&self, guild_id: GuildId) -> bool {
        self.asks.clear(guild_id)
    }

    /// Whether `user` may answer; a spoken question is the channel's, checked by the caller.
    pub fn ask_belongs_to(&self, guild_id: GuildId, user: UserId) -> bool {
        match self.ask_for(guild_id).map(|ask| ask.owner) {
            Some(Owner::User(asker)) => asker == user,
            Some(Owner::Voice) => true,
            None => false,
        }
    }

    /// The guild's outstanding search, if it has one.
    fn ask_for(&self, guild_id: GuildId) -> Option<Ask> {
        self.asks.lock().get(&guild_id).cloned()
    }

    /// Edits the outstanding search's message in place.
    async fn redraw_ask(
        &self,
        ctx: &SerenityContext,
        guild_id: GuildId,
        message: dashboard::View,
    ) -> bool {
        let Some((channel_id, message_id)) = self.ask_for(guild_id).and_then(|a| a.posted) else {
            return false;
        };
        match channel_id
            .edit_message(&ctx.http, message_id, message.clone().into_edit())
            .await
        {
            Ok(_) => true,
            Err(e) => {
                tracing::warn!(guild_id = %guild_id, error = %format!("{e:#}"), "could not turn the search page");
                false
            }
        }
    }
}

/// What a songbird ending needs, owned so the callback can outlive its caller.
struct Follower {
    manager: Arc<Songbird>,
    queues: Queues,
    tracks: Tracks,
    board: Board,
    coordination: Coordination,
    ctx: SerenityContext,
    config: Config,
    guild_id: GuildId,
}

impl Follower {
    /// Draws or updates this guild's dashboard.
    async fn draw_dashboard(&self) {
        draw(&self.board, &self.ctx, &self.config, self.guild_id, None).await;
    }
}

/// Wakes the advance task when a track ends; songbird's thread only sends on a channel.
struct Ended(mpsc::UnboundedSender<()>);

#[async_trait]
impl EventHandler for Ended {
    async fn act(&self, _ctx: &EventContext<'_>) -> Option<Event> {
        let _ = self.0.send(());
        None
    }
}

/// [`start_track`], moving past songs that cannot start, so a dead one never
/// stays "current" with nothing playing and every later request queued behind it.
/// Returns the handle, the advance that ended up playing, and the titles skipped.
async fn start_playable(
    manager: &Songbird,
    guild_id: GuildId,
    queue: &Queue,
    mut advanced: Advanced,
    timing: &Timing,
) -> (Option<TrackHandle>, Advanced, Vec<String>) {
    let mut skipped = Vec::new();
    loop {
        if let Some(handle) = start_track(manager, guild_id, queue, &advanced, timing).await {
            return (Some(handle), advanced, skipped);
        }
        let Some(failed) = advanced.current.as_ref() else {
            return (None, advanced, skipped);
        };
        skipped.push(failed.title.clone());
        match queue.skip_failed(advanced.generation) {
            Some(next) => advanced = next,
            None => return (None, advanced, skipped),
        }
    }
}

/// Stops the playing song and starts the next, caching its audio as it plays.
///
/// Returns `None` when the queue is dry, the bot left, or the fetch failed. The old
/// file is left for `fetch::prune`; locks with `await` since `blocking_lock` panics here.
async fn start_track(
    manager: &Songbird,
    guild_id: GuildId,
    queue: &Queue,
    advanced: &Advanced,
    timing: &Timing,
) -> Option<TrackHandle> {
    let started = std::time::Instant::now();
    let call = manager.get(guild_id)?;
    // Stop first: resolving the next source may mean a download.
    call.lock().await.stop();
    let song = advanced.current.as_ref()?;

    let in_use = queue.in_use();
    let source = match fetch::resolve(&song.source, &song.url, &in_use, timing).await {
        Ok(source) => source,
        Err(e) => {
            tracing::warn!(
                guild_id = %guild_id,
                title = %song.title,
                error = %format!("{e:#}"),
                "this song has no audio to play"
            );
            return None;
        }
    };

    // Cache while playing so a repeat is instant; detached since nobody waits on it.
    if let Some(stream) = fetch::Streamable::of(&song.source) {
        let Source::Stream { cache, .. } = &song.source else {
            unreachable!("a stream always carries a cache path")
        };
        let (cache, watch_url) = (cache.clone(), song.url.clone());
        let in_use = in_use.clone();
        tokio::spawn(async move {
            if let Err(e) =
                fetch::save(&stream, &cache, &watch_url, &in_use, &Timing::default()).await
            {
                tracing::warn!(
                    cache = %cache.display(),
                    error = %format!("{e:#}"),
                    "the streamed audio could not be cached, so a repeat will fetch it again"
                );
            }
        });
    }

    let (input, route) = match &source {
        Source::Stream {
            url,
            headers,
            length,
            ..
        } => {
            let mut request =
                HttpRequest::new_with_headers(reqwest::Client::new(), url.clone(), headers.clone());
            // YouTube serves this URL only for a closed range, which songbird needs a length for.
            request.content_length = Some(*length);
            (Input::from(request), "stream")
        }
        Source::File(path) => (Input::from(FileInput::new(path.clone())), "file"),
    };
    timing.set_route(route);
    tracing::info!(guild_id = %guild_id, title = %song.title, route, "playing");

    let handle = call.lock().await.play_input(input);
    let _ = handle.add_event(
        Event::Track(TrackEvent::Playable),
        FirstAudio {
            started,
            route,
            title: song.title.clone(),
        },
    );
    Some(handle)
}

/// Logs time to first audio: when songbird has decoded the source and can mix it.
struct FirstAudio {
    started: std::time::Instant,
    /// `stream` or `file`, so the log says which path the number came from.
    route: &'static str,
    title: String,
}

#[async_trait]
impl EventHandler for FirstAudio {
    async fn act(&self, _ctx: &EventContext<'_>) -> Option<Event> {
        tracing::info!(
            title = %self.title,
            route = self.route,
            time_to_first_audio_ms = self.started.elapsed().as_millis() as u64,
            "first audio"
        );
        None
    }
}

/// Draws the dashboard and keeps its progress bar moving.
///
/// `page` None keeps the current page. Any timer is stopped first so it can't paint
/// stale state; a new one starts only for a moving bar.
async fn draw(
    board: &Board,
    ctx: &SerenityContext,
    config: &Config,
    guild_id: GuildId,
    page: Option<Page>,
) {
    board.refreshes.stop(guild_id);
    if !paint(board, ctx, config, guild_id, page).await {
        return;
    }
    let (timer_board, ctx, config) = (board.clone(), ctx.clone(), config.clone());
    let timer = tokio::spawn(async move {
        loop {
            tokio::time::sleep(REFRESH_EVERY).await;
            if !paint(&timer_board, &ctx, &config, guild_id, None).await {
                break;
            }
        }
    });
    board.refreshes.set(guild_id, timer.abort_handle());
}

/// Draws a guild's dashboard once; returns whether its bar is worth refreshing.
async fn paint(
    board: &Board,
    ctx: &SerenityContext,
    config: &Config,
    guild_id: GuildId,
    page: Option<Page>,
) -> bool {
    let Some(channel) = resolve_alert_channel(ctx, config, guild_id) else {
        return false;
    };
    let dashboards = &board.dashboards;
    let page = page.unwrap_or_else(|| dashboards.page(guild_id));
    let queue = board.queues.get(guild_id);
    let current = queue.current();
    let paused = queue.is_paused();
    // From songbird, whose clock stops while paused.
    let position = match (&current, board.tracks.get(guild_id)) {
        (Some(song), Some(track)) if page == Page::NowPlaying && song.duration_secs.is_some() => {
            track
                .get_info()
                .await
                .ok()
                .map(|state| state.position.as_secs_f64())
        }
        _ => None,
    };
    let view = match page {
        Page::Queue { selected } => dashboard::queue_view(&queue.waiting(), selected),
        Page::NowPlaying => match &current {
            Some(song) => dashboard::dashboard(
                song,
                position,
                paused,
                &queue.pending_songs(),
                queue.is_repeat(),
            ),
            None => dashboard::idle_dashboard(&config.wake_words),
        },
    };

    match dashboards.get(guild_id) {
        Some(message_id) => match channel
            .edit_message(&ctx.http, message_id, view.clone().into_edit())
            .await
        {
            // Remember the page, or the next song ending flips the queue back.
            Ok(_) => dashboards.set(guild_id, channel, message_id, page),
            Err(e) => {
                // Message deleted or access lost: post a new one and delete the old.
                tracing::warn!(guild_id = %guild_id, error = %format!("{e:#}"), "failed to update the dashboard");
                dashboards.forget(guild_id);
                let _ = channel.delete_message(&ctx.http, message_id).await;
                match channel.send_message(&ctx.http, view.into_message()).await {
                    Ok(sent) => dashboards.set(guild_id, channel, sent.id, page),
                    Err(_) => return false,
                }
            }
        },
        None => match channel.send_message(&ctx.http, view.into_message()).await {
            Ok(sent) => dashboards.set(guild_id, channel, sent.id, page),
            Err(e) => {
                tracing::warn!(guild_id = %guild_id, error = %format!("{e:#}"), "failed to post the dashboard");
                return false;
            }
        },
    }
    worth_refreshing(page, position.is_some(), paused)
}

/// Builds the tone as an in-memory 16-bit PCM WAV, faded to avoid clicks.
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

/// The feedback channel: configured alert channel, else system channel, else first text channel.
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
#[path = "tests/player.rs"]
mod tests;
