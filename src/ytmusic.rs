//! YouTube Music's own song catalogue, read straight from the innertube
//! endpoint the web app posts to.
//!
//! Why this exists when the pipeline already searches YouTube for a video: the
//! ordinary search returns *videos*, and the titles on those are written by
//! uploaders -- "Dracula (Official Video)", "Dracula but it\'s the 10 minute
//! version", a two-second clip. None of that is a list of songs. The `songs`
//! filter on this endpoint returns the label\'s own entries instead: the title
//! as the label spelled it, the artist from the catalogue rather than from a
//! channel name, and the running time of the audio track itself.
//!
//! It is one unauthenticated POST. There is no key, no account and no yt-dlp,
//! and the response carries a `videoId`, so the hit is playable straight away.
//!
//! The request shape and the `songs` filter parameter are taken from
//! `ytmusicapi`\'s current source (`ytmusicapi/mixins/search.py` and
//! `ytmusicapi/parsers/search.py`), which is the maintained description of this
//! endpoint.

use crate::resolve::{CatalogProvider, CatalogQuery, CatalogTrack, ProviderError, Rate};
use anyhow::Context as _;
use async_trait::async_trait;
use serde_json::{Value, json};
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::warn;

/// The search endpoint, as the WEB_REMIX web app calls it.
const SEARCH_URL: &str = "https://music.youtube.com/youtubei/v1/search?alt=json";

/// The site the request is coming from, which innertube checks against the
/// `Origin` header.
const ORIGIN: &str = "https://music.youtube.com";

/// The `songs` filter, base64 protobuf: content type songs, spelling
/// correction on.
///
/// Taken verbatim from `get_search_params("songs", None, False)` in
/// `ytmusicapi/parsers/search.py`, where it is assembled as `EgWKAQ` + `II` +
/// `AWoMEA4QChADEAQQCRAF`. The trailing part is what lets YouTube Music
/// autocorrect a misheard word, which is the whole reason a misspelt request
/// gets here at all.
const SONGS_PARAMS: &str = "EgWKAQIIAWoMEA4QChADEAQQCRAF";

/// The one `musicVideoType` that means "this is the studio audio, with no
/// video".
///
/// The others are a user upload, or an official music video whose length is the
/// length of the video. Only the audio track's running time is the running time
/// the matcher trusts, so this is what [`CatalogTrack::official`] records.
const AUDIO_TRACK: &str = "MUSIC_VIDEO_TYPE_ATV";

/// YouTube Music's catalogue, over its public innertube search.
pub struct YtMusic {
    /// Held rather than built per call: see [`crate::resolve`]'s note on the
    /// catalog HTTP client, and innertube is HTTP/2 and gzip -- a fresh client
    /// would redo that negotiation on every spoken command.
    client: reqwest::Client,
}

impl YtMusic {
    pub fn new() -> anyhow::Result<Self> {
        Ok(Self {
            client: crate::resolve::catalog_http_client()?,
        })
    }
}

#[async_trait]
impl CatalogProvider for YtMusic {
    fn name(&self) -> &'static str {
        "ytmusic"
    }

    fn rate(&self) -> Rate {
        // Innertube publishes no limit, which is not the same as no limit: a
        // burst of a couple and one a second is well inside what the web app
        // itself uses, and this fan-out only ever sends a handful of queries.
        Rate::new(2.0, 1.0)
    }

    async fn search(&self, query: &CatalogQuery) -> Result<Vec<CatalogTrack>, ProviderError> {
        let response = self
            .client
            .post(SEARCH_URL)
            .header("origin", ORIGIN)
            .header("accept", "*/*")
            .json(&request(&query.text()))
            .send()
            .await
            .map_err(|_| ProviderError::Unavailable)?;
        let status = response.status();
        if status.as_u16() == 429 {
            return Err(ProviderError::RateLimited);
        }
        if !status.is_success() {
            return Err(ProviderError::Unavailable);
        }

        let body: Value = response.json().await.map_err(|_| ProviderError::BadResponse)?;
        tracks(&body)
    }
}

/// The POST body: the query, the songs filter, and the WEB_REMIX client
/// context.
///
/// `query.as_loose()` everywhere else in the resolver is the same idea -- innertube
/// has no fielded syntax, so a confirmation query is the free-text query it
/// would have been anyway.
fn request(query: &str) -> Value {
    json!({
        "query": query,
        "params": SONGS_PARAMS,
        "context": {
            "client": {
                "clientName": "WEB_REMIX",
                "clientVersion": client_version(),
                "hl": "en",
            },
            "user": {},
        },
    })
}

/// Today's WEB_REMIX client version, `1.<YYYYMMDD>.01.00`.
///
/// Innertube dates its web clients daily, and `ytmusicapi` rebuilds this string
/// from the clock on every call (`initialize_context`). A pinned version works
/// today and is the kind of thing that silently degrades next year, so the
/// eight lines below are cheaper than the bug.
fn client_version() -> String {
    let days = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since| since.as_secs() / 86_400)
        .unwrap_or_default() as i64;
    // Civil-from-days, Howard Hinnant's algorithm.
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_year_of_era(day_of_era, year_of_era);
    let month_part = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_part + 2) / 5 + 1;
    let month = if month_part < 10 { month_part + 3 } else { month_part - 9 };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    format!("1.{year:04}{month:02}{day:02}.01.00")
}

fn day_of_year_of_era(day_of_era: i64, year_of_era: i64) -> i64 {
    day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100)
}

/// One step down a JSON object, or `None` when the field is not there.
///
/// Innertube nests this response seven levels deep and adds and drops branches
/// freely, so every read is a lookup rather than an index: an unexpected shape
/// has to be a `None` this code can report, never a panic on a guild's voice
/// connection.
fn dig<'a>(value: &'a Value, path: &[&str]) -> Option<&'a Value> {
    path.iter().try_fold(value, |value, key| value.get(*key))
}

/// A search response, read.
fn tracks(body: &Value) -> Result<Vec<CatalogTrack>, ProviderError> {
    let Some(contents) = body.get("contents") else {
        // Innertube's way of saying there was nothing to find, and it is a
        // miss rather than a failure: opening the breaker over an unheard-of
        // title would stop the provider for the requests that would work.
        return Ok(Vec::new());
    };
    let sections = dig(
        contents,
        &[
            "tabbedSearchResultsRenderer",
            "tabs",
        ],
    )
    .and_then(|tabs| tabs.get(0))
    .and_then(|tab| tab.get("tabRenderer"))
    .and_then(|tab| tab.get("content"))
    .and_then(|content| content.get("sectionListRenderer"))
    .and_then(|list| list.get("contents"))
    .and_then(Value::as_array)
    .ok_or_else(|| unexpected("sectionListRenderer.contents"))?;

    let mut found = Vec::new();
    let mut first_miss = None;
    for row in sections
        .iter()
        .filter_map(|section| dig(section, &["musicShelfRenderer", "contents"]))
        .filter_map(Value::as_array)
        .flatten()
    {
        match song(row) {
            Ok(song) => found.push(song.into()),
            // One unusable row must not throw away the nineteen good ones, but
            // a shape change is worth seeing in the logs -- once, naming the
            // field, rather than once per row.
            Err(field) => first_miss.get_or_insert(field),
        };
    }
    if let Some(field) = first_miss {
        warn!(
            provider = "ytmusic",
            missing = field,
            "a YouTube Music search row was skipped"
        );
    }
    Ok(found)
}

/// Reports a field the response did not have, and the error the caller turns
/// into one fewer opinion.
fn unexpected(field: &str) -> ProviderError {
    warn!(
        provider = "ytmusic",
        missing = field,
        "unexpected YouTube Music response shape"
    );
    ProviderError::BadResponse
}

/// One search row, read.
struct Song {
    title: String,
    artists: Vec<String>,
    duration_secs: Option<f64>,
    video_id: String,
    music_video_type: Option<String>,
}

/// Reads one `musicResponsiveListItemRenderer`, or names the field it lacked.
///
/// The `Err` is the missing field rather than a message so the caller can log
/// exactly what moved.
fn song(row: &Value) -> Result<Song, &'static str> {
    let columns = row
        .get("flexColumns")
        .and_then(Value::as_array)
        .ok_or("flexColumns")?;

    // Column 0 is the title, column 1 the artist / album / length line, and
    // column 2 (when there is one) the play count, which this code ignores.
    let runs = |index: usize| -> Option<&Vec<Value>> {
        columns
            .get(index)
            .and_then(|column| column.get("musicResponsiveListItemFlexColumnRenderer"))
            .and_then(|column| column.get("text"))
            .and_then(|text| text.get("runs"))
            .and_then(Value::as_array)
    };

    let title = runs(0)
        .and_then(|runs| runs.first())
        .and_then(|run| run.get("text"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|title| !title.is_empty())
        .ok_or("title")?
        .to_string();

    let mut artists = Vec::new();
    let mut duration_secs = None;
    if let Some(runs) = runs(1) {
        for run in runs {
            let text = run.get("text").and_then(Value::as_str).unwrap_or_default();
            // The separator runs between the fields are unlinked text, so a
            // run with a link is a real field: `MPRE…` is an album, any other
            // browse id is an artist, and an unlinked `m:ss` is the length.
            match dig(run, &["navigationEndpoint", "browseEndpoint", "browseId"])
                .and_then(Value::as_str)
            {
                Some(browse_id) if !browse_id.starts_with("MPRE") => artists.push(text.to_string()),
                Some(_) => {}
                None => duration_secs = duration_secs.or_else(|| seconds(text)),
            }
        }
    }

    let watch = dig(
        row,
        &[
            "overlay",
            "musicItemThumbnailOverlayRenderer",
            "content",
            "musicPlayButtonRenderer",
            "playNavigationEndpoint",
            "watchEndpoint",
        ],
    )
    .ok_or("watchEndpoint")?;
    let video_id = watch
        .get("videoId")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .ok_or("videoId")?
        .to_string();
    let music_video_type = dig(
        watch,
        &[
            "watchEndpointMusicSupportedConfigs",
            "watchEndpointMusicConfig",
            "musicVideoType",
        ],
    )
    .and_then(Value::as_str)
    .map(str::to_string);

    Ok(Song {
        title,
        artists,
        duration_secs,
        video_id,
        music_video_type,
    })
}

/// `"3:26"` and `"1:02:03"` as seconds, or `None` for anything else.
///
/// Deliberately strict: a comma-grouped number ("2,343") is a play count, not a
/// running time, and guessing at it would put the wrong length in front of the
/// matcher, which trusts length more than anything else.
fn seconds(text: &str) -> Option<f64> {
    let mut total = 0.0_f64;
    let mut parts = 0;
    for part in text.trim().split(':') {
        total = total * 60.0 + part.parse::<f64>().ok()?;
        parts += 1;
    }
    (parts > 1).then_some(total)
}

impl From<Song> for CatalogTrack {
    fn from(song: Song) -> Self {
        Self {
            artist: song.artists.join(", "),
            title: song.title,
            duration_secs: song.duration_secs,
            id: song.video_id.clone(),
            provider: "ytmusic",
            video_id: Some(song.video_id),
            official: song.music_video_type.as_deref() == Some(AUDIO_TRACK),
            popularity: None,
        }
    }
}

/// A single search, against the live endpoint.
///
/// Ignored by default: it is the only way to notice that YouTube has moved a
/// field, and the recorded fixture is what the rest of the tests check.
#[cfg(test)]
mod tests {
    use super::*;

    fn recorded() -> Value {
        serde_json::from_str(include_str!("../tests/fixtures/ytmusic_dracula.json"))
            .expect("the recorded response decodes")
    }

    #[test]
    fn a_recorded_response_becomes_playable_tracks() {
        let tracks = tracks(&recorded()).expect("the recorded shape parses");
        assert_eq!(tracks.len(), 3, "the 'About these results' panel is not a song");

        let dracula = &tracks[0];
        assert_eq!(dracula.artist, "Tame Impala");
        assert_eq!(dracula.title, "Dracula");
        assert_eq!(dracula.duration_secs, Some(206.0), "3:26 becomes seconds");
        assert_eq!(dracula.provider, "ytmusic");
        assert_eq!(dracula.id, "cuMuMnCRfqk");
        assert_eq!(dracula.video_id.as_deref(), Some("cuMuMnCRfqk"));
        assert!(dracula.official, "an album audio track, not a music video");
        assert_eq!(dracula.popularity, None);

        assert_eq!(tracks[1].artist, "Token");
        assert_eq!(tracks[1].duration_secs, Some(160.0));
        assert_eq!(tracks[2].title, "Dracula (feat. JENNIE)");
        assert_eq!(
            tracks[2].duration_secs,
            Some(210.0),
            "the album name is not mistaken for the artist"
        );
    }

    /// The length is the signal the whole resolver is built on, so it has to be
    /// right for every arity innertube spells out.
    #[test]
    fn a_running_time_becomes_seconds_at_every_arity() {
        assert_eq!(seconds("3:26"), Some(206.0));
        assert_eq!(seconds("1:02:03"), Some(3723.0));
        assert_eq!(seconds(" 4:05 "), Some(245.0));
    }

    /// Innertube puts a play count and a comma-grouped figure in the same
    /// column. Reading one as a running time would put a wrong length in front
    /// of the matcher, which trusts length above everything else.
    #[test]
    fn something_that_is_not_a_running_time_is_not_one() {
        assert_eq!(seconds("213M plays"), None);
        assert_eq!(seconds("2,343"), None);
        assert_eq!(seconds("2025"), None);
        assert_eq!(seconds(""), None);
        assert_eq!(seconds(" • "), None);
    }

    /// A music video's length is the length of the video, so a row without an
    /// audio track must not claim to be one.
    #[test]
    fn a_music_video_is_not_official() {
        let row = serde_json::json!({
            "flexColumns": [
                {"musicResponsiveListItemFlexColumnRenderer": {"text": {"runs": [{"text": "Dracula"}]}}},
                {"musicResponsiveListItemFlexColumnRenderer": {"text": {"runs": [
                    {"text": "Tame Impala", "navigationEndpoint": {"browseEndpoint": {"browseId": "UC123"}}},
                    {"text": " • "},
                    {"text": "3:59"},
                ]}}},
            ],
            "overlay": {"musicItemThumbnailOverlayRenderer": {"content": {"musicPlayButtonRenderer": {
                "playNavigationEndpoint": {"watchEndpoint": {
                    "videoId": "abcd",
                    "watchEndpointMusicSupportedConfigs": {"watchEndpointMusicConfig": {
                        "musicVideoType": "MUSIC_VIDEO_TYPE_OMV"}}}}}}}},
        });
        let track: CatalogTrack = song(&row).expect("the row parses").into();
        assert!(!track.official);
        assert_eq!(track.duration_secs, Some(239.0));
        assert_eq!(track.video_id.as_deref(), Some("abcd"));
    }

    /// A row with no length is still worth having: the title and artist are
    /// what the matcher needs and what the follow-up YouTube search is built
    /// from.
    #[test]
    fn a_row_with_no_running_time_is_kept() {
        let body = serde_json::json!({
            "contents": {"tabbedSearchResultsRenderer": {"tabs": [{"tabRenderer": {"content": {
                "sectionListRenderer": {"contents": [{"musicShelfRenderer": {"contents": [{
                    "flexColumns": [
                        {"musicResponsiveListItemFlexColumnRenderer": {"text": {"runs": [{"text": "Dracula"}]}}},
                        {"musicResponsiveListItemFlexColumnRenderer": {"text": {"runs": [
                            {"text": "Tame Impala",
                             "navigationEndpoint": {"browseEndpoint": {"browseId": "UC123"}}}]}}},
                    ],
                    "overlay": {"musicItemThumbnailOverlayRenderer": {"content": {"musicPlayButtonRenderer": {
                        "playNavigationEndpoint": {"watchEndpoint": {"videoId": "abcd"}}}}}},
                }]}}]}}}}}}}}}}});
        let tracks = tracks(&body).expect("a missing length is not a broken shape");
        assert_eq!(tracks.len(), 1);
        assert_eq!(tracks[0].duration_secs, None);
        assert_eq!(tracks[0].artist, "Tame Impala");
        assert!(!tracks[0].official, "no type means no claim to be an audio track");
    }

    /// Innertube is not a documented API. A shape this code cannot read has to
    /// come back as a provider error, so the fan-out carries on without it,
    /// rather than taking the bot down mid-song.
    #[test]
    fn an_unreadable_body_is_a_provider_error_not_a_panic() {
        for body in [
            serde_json::json!({}),
            serde_json::json!({"contents": {"itemSectionRenderer": {}}}),
            serde_json::json!({"contents": {"tabbedSearchResultsRenderer": {"tabs": []}}}),
            serde_json::json!({"contents": {"tabbedSearchResultsRenderer": {"tabs": [{"tabRenderer":
                {"content": {"sectionListRenderer": {"contents": [{"musicShelfRenderer": {"contents":
                    [{"musicResponsiveListItemRenderer": {"flexColumns": "not a list"}}]}}]}}}}}]}}}),
            serde_json::json!({"contents": {"tabbedSearchResultsRenderer": {"tabs": [{"tabRenderer":
                {"content": {"sectionListRenderer": {"contents": [{"musicShelfRenderer": {"contents":
                    [{"musicResponsiveListItemRenderer": {}}]}}]}}}}}]}}}),
            serde_json::json!({"contents": {"tabbedSearchResultsRenderer": {"tabs": [{"tabRenderer":
                {"content": {"sectionListRenderer": {"contents": [{"musicShelfRenderer": {"contents":
                    [{"musicResponsiveListItemRenderer": {"flexColumns": [], "overlay": {}}}]}}]}}}}}]}}}),
        ] {
            assert!(
                matches!(tracks(&body), Err(ProviderError::BadResponse)),
                "unreadable: {body}"
            );
        }
    }

    /// No `contents` at all is how innertube reports that nothing matched, and
    /// a title nobody has heard of must not be treated as a broken provider.
    #[test]
    fn nothing_found_is_a_miss_not_a_failure() {
        assert_eq!(
            tracks(&serde_json::json!({"responseContext": {}})).expect("a miss"),
            Vec::new()
        );
    }

    #[test]
    fn the_request_is_a_songs_filtered_web_remix_search() {
        let body = request("dracula tame impala");
        assert_eq!(body["query"], "dracula tame impala");
        assert_eq!(body["params"], SONGS_PARAMS);
        assert_eq!(body["context"]["client"]["clientName"], "WEB_REMIX");
        assert_eq!(body["context"]["client"]["hl"], "en");
    }

    /// The version is dated, and a wrong one is the kind of thing that only
    /// shows up as quietly worse results.
    #[test]
    fn the_client_version_is_todays_web_remix_release() {
        let version = client_version();
        assert!(version.starts_with("1."), "unhelpful: {version}");
        assert!(version.ends_with(".01.00"), "unhelpful: {version}");
        let stamp = &version[2..10];
        assert_eq!(stamp.len(), 8);
        assert!(stamp.chars().all(|c| c.is_ascii_digit()), "unhelpful: {version}");
        assert!(stamp.starts_with("20"), "unhelpful: {version}");
    }

    #[tokio::test]
    #[ignore = "hits the live endpoint; run with --ignored"]
    async fn the_live_endpoint_answers() {
        let ytmusic = YtMusic::new().expect("a client");
        assert_eq!(ytmusic.name(), "ytmusic");
        let hits = ytmusic
            .search(&CatalogQuery::Loose("dracula tame impala".to_string()))
            .await
            .expect("a live search");
        assert!(!hits.is_empty(), "the live endpoint returned nothing");
        for hit in &hits {
            println!(
                "{:>7.0?}s  {:<28} {:<28} {}",
                hit.duration_secs.unwrap_or_default(),
                hit.artist,
                hit.title,
                hit.video_id.as_deref().unwrap_or("-"),
            );
        }
        let first = &hits[0];
        assert_eq!(first.artist, "Tame Impala");
        assert_eq!(first.title, "Dracula");
        assert!(first.official);
        assert_eq!(first.duration_secs, Some(206.0));
        assert_eq!(first.video_id.as_deref(), Some("cuMuMnCRfqk"));
    }
}
