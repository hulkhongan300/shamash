//! Whether a spoken request was for a whole album, and which one.
//!
//! Two ways in. A request that says "album" ("play the album 17 by
//! xxxtentacion", see [`crate::parser::album_request`]) is for an album, and
//! only the albums are searched. A request that does not say it ("play 17 by
//! xxxtentacion") is searched for as a song *and* as an album at the same time,
//! and [`settle`] decides between them:
//!
//! | the songs | the artist was named | what happens |
//! | --- | --- | --- |
//! | nothing matched | yes | the album plays |
//! | nothing matched | no | "Which one?", with the album on offer |
//! | several songs | either | "Which one?", the album first, then the songs |
//! | one song | yes | "Which one?": the song or the album |
//! | one song | no | the song plays |
//!
//! An album only counts here when its title is what was said, near enough to
//! survive a mishearing ("blond" for *Blonde*) but no looser: *LOOK AT ME: THE
//! ALBUM* must not turn "play Look at Me by XXXTENTACION" into a question.
//! Singles never count unless "album" was said, because YouTube Music lists
//! most songs as a single of their own.
//!
//! A song picked from the question is remembered like any other pick, so a
//! song that shares its album's name is asked about once, not every time.

use crate::matcher::{self, Verdict};
use crate::parser::PlayRequest;
use crate::resolve::{CatalogAlbum, CatalogTrack, ReleaseKind};
use crate::search::Resolved;

/// What to ask the catalog's album search for a request: the title and the
/// artist without the "by" between them, the way the song search asks.
pub fn query(request: &PlayRequest) -> String {
    match request.splits.first() {
        Some((title, artist)) => format!("{title} {artist}"),
        None => request.query.clone(),
    }
}

/// How closely an album's title has to match what was said for it to be
/// considered when "album" was not said.
///
/// The matcher's own gate lets a title through at 0.65, which is right for
/// songs and far too loose for interrupting a song request with a question.
const UNSAID_TITLE: f32 = 0.9;

/// What the catalog's releases say about the words.
#[derive(Debug, Clone, PartialEq)]
pub enum Found {
    /// One release answers to them.
    One(CatalogAlbum),
    /// Several do, and the words do not say which.
    Several(Vec<CatalogAlbum>),
    /// None does.
    Nothing,
}

/// Which release `said` names.
///
/// `named_as_album` is a request that said "album": then a single counts too,
/// since the speaker asked for that release, and the title only has to clear
/// the matcher's usual bar.
pub fn find(said: &str, albums: &[CatalogAlbum], named_as_album: bool) -> Found {
    let eligible: Vec<&CatalogAlbum> = albums
        .iter()
        .filter(|album| named_as_album || album.kind != ReleaseKind::Single)
        .collect();
    let found = match matcher::decide(said, &eligible, |album| {
        (album.artist.as_str(), album.title.as_str())
    }) {
        Verdict::Confident(album) => Found::One((*album).clone()),
        Verdict::Ambiguous(albums) => {
            Found::Several(albums.into_iter().map(|album| (*album).clone()).collect())
        }
        Verdict::NoMatch => Found::Nothing,
    };
    match found {
        Found::One(album)
            if !named_as_album
                && matcher::score(said, &album.artist, &album.title).title < UNSAID_TITLE =>
        {
            Found::Nothing
        }
        // A question about albums is only worth asking when "album" was said.
        Found::Several(_) if !named_as_album => Found::Nothing,
        found => found,
    }
}

/// What to do with a request that was searched for as a song and an album.
#[derive(Debug, Clone, PartialEq)]
pub enum Settle {
    /// It was a song, or nothing at all: carry on as if no album had been
    /// looked for.
    Song,
    /// Play the album.
    Album(CatalogAlbum),
    /// Ask, with the album as the first choice and these songs after it.
    Ask {
        album: CatalogAlbum,
        songs: Vec<CatalogTrack>,
    },
}

/// Decides between the song the words resolved to and the album they name.
/// See the table in the module docs.
pub fn settle(said: &str, songs: &Resolved, album: Found) -> Settle {
    let Found::One(album) = album else {
        return Settle::Song;
    };
    let artist_named = matcher::score(said, &album.artist, &album.title).artist > 0.0;
    match songs {
        Resolved::Unknown if artist_named => Settle::Album(album),
        Resolved::Unknown => Settle::Ask {
            album,
            songs: Vec::new(),
        },
        Resolved::Ask(tracks) => Settle::Ask {
            album,
            songs: tracks.clone(),
        },
        Resolved::Track { track, .. } if artist_named => Settle::Ask {
            album,
            songs: vec![track.clone()],
        },
        Resolved::Track { .. } => Settle::Song,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn album(artist: &str, title: &str, kind: ReleaseKind) -> CatalogAlbum {
        CatalogAlbum {
            title: title.to_string(),
            artist: artist.to_string(),
            kind,
            year: None,
            playlist_id: format!("OLAK5uy_{}", title.replace(' ', "")),
        }
    }

    fn song(artist: &str, title: &str) -> CatalogTrack {
        CatalogTrack {
            artist: artist.to_string(),
            title: title.to_string(),
            duration_secs: Some(200.0),
            popularity: None,
            video_id: Some("abcdefghijk".to_string()),
        }
    }

    fn track(artist: &str, title: &str) -> Resolved {
        Resolved::Track {
            track: song(artist, title),
            durations: vec![200.0],
        }
    }

    /// What YouTube Music's album search returned for "17 xxxtentacion".
    fn xxxtentacion() -> Vec<CatalogAlbum> {
        vec![
            album("XXXTENTACION", "17", ReleaseKind::Album),
            album("XXXTENTACION", "?", ReleaseKind::Album),
            album("XXXTENTACION", "Bad Vibes Forever", ReleaseKind::Album),
            album("XXXTENTACION", "LOOK AT ME: THE ALBUM", ReleaseKind::Album),
        ]
    }

    #[test]
    fn an_album_named_with_its_artist_is_found() {
        assert_eq!(
            find("17 by xxxtentacion", &xxxtentacion(), false),
            Found::One(album("XXXTENTACION", "17", ReleaseKind::Album))
        );
    }

    /// The song "Look at Me!" shares most of its name with an album, which
    /// must not make the song request a question.
    #[test]
    fn an_album_only_close_to_what_was_said_is_not_found() {
        assert_eq!(
            find("look at me by xxxtentacion", &xxxtentacion(), false),
            Found::Nothing
        );
    }

    #[test]
    fn a_misheard_album_title_is_still_found() {
        let albums = [album("Frank Ocean", "Blonde", ReleaseKind::Album)];
        assert!(matches!(
            find("blond by frank ocean", &albums, false),
            Found::One(_)
        ));
    }

    /// "Dracula" is a single of its own on YouTube Music. Asked for as a song,
    /// it is the song.
    #[test]
    fn a_single_is_an_album_only_when_album_was_said() {
        let albums = [album("Tame Impala", "Dracula", ReleaseKind::Single)];
        assert_eq!(
            find("dracula by tame impala", &albums, false),
            Found::Nothing
        );
        assert!(matches!(
            find("dracula by tame impala", &albums, true),
            Found::One(_)
        ));
    }

    /// Two albums with one title by different artists: a question, but only
    /// when "album" was said.
    #[test]
    fn two_artists_with_one_album_title_are_a_question_when_album_was_said() {
        let albums = [
            album("Tame Impala", "Currents", ReleaseKind::Album),
            album("Someone Else", "Currents", ReleaseKind::Album),
        ];
        assert!(matches!(find("currents", &albums, true), Found::Several(_)));
        assert_eq!(find("currents", &albums, false), Found::Nothing);
    }

    #[test]
    fn nothing_close_is_nothing() {
        assert_eq!(
            find("the less i know the better", &xxxtentacion(), true),
            Found::Nothing
        );
    }

    fn seventeen() -> Found {
        Found::One(album("XXXTENTACION", "17", ReleaseKind::Album))
    }

    /// The request in the issue: no song is called 17, the artist was named,
    /// so the album plays without a question.
    #[test]
    fn no_song_and_a_named_artist_plays_the_album() {
        assert_eq!(
            settle("17 by xxxtentacion", &Resolved::Unknown, seventeen()),
            Settle::Album(album("XXXTENTACION", "17", ReleaseKind::Album))
        );
    }

    #[test]
    fn no_song_and_no_artist_asks() {
        let currents = Found::One(album("Tame Impala", "Currents", ReleaseKind::Album));
        assert!(matches!(
            settle("currents", &Resolved::Unknown, currents),
            Settle::Ask { songs, .. } if songs.is_empty()
        ));
    }

    /// "Thriller" is a song and an album by the same artist.
    #[test]
    fn a_song_and_an_album_of_the_same_name_asks() {
        let thriller = Found::One(album("Michael Jackson", "Thriller", ReleaseKind::Album));
        assert_eq!(
            settle(
                "thriller by michael jackson",
                &track("Michael Jackson", "Thriller"),
                thriller.clone()
            ),
            Settle::Ask {
                album: album("Michael Jackson", "Thriller", ReleaseKind::Album),
                songs: vec![song("Michael Jackson", "Thriller")],
            }
        );
        // With no artist named, the song the catalog was sure of plays.
        assert_eq!(
            settle("thriller", &track("Michael Jackson", "Thriller"), thriller),
            Settle::Song
        );
    }

    #[test]
    fn several_songs_put_the_album_first_among_them() {
        let songs = vec![song("A", "Currents"), song("B", "Currents")];
        let currents = Found::One(album("Tame Impala", "Currents", ReleaseKind::Album));
        assert_eq!(
            settle("currents", &Resolved::Ask(songs.clone()), currents),
            Settle::Ask {
                album: album("Tame Impala", "Currents", ReleaseKind::Album),
                songs,
            }
        );
    }

    #[test]
    fn no_album_leaves_the_song_alone() {
        for songs in [
            Resolved::Unknown,
            Resolved::Ask(vec![song("A", "B")]),
            track("Tame Impala", "Dracula"),
        ] {
            assert_eq!(
                settle("dracula by tame impala", &songs, Found::Nothing),
                Settle::Song
            );
        }
    }

    /// The whole decision against the live catalog: songs and albums looked
    /// up the way a spoken request looks them up, then settled.
    #[tokio::test]
    #[ignore = "hits the live YouTube Music endpoint; run with --ignored"]
    async fn live_requests_settle_the_way_they_should() {
        use crate::resolve::{Catalog, YtMusic};
        let catalog = Catalog::with_budget(
            vec![Box::new(YtMusic::new().expect("an HTTP client"))],
            2_500,
        );
        for (said, expected) in [
            ("17 by xxxtentacion", "album"),
            ("currents by tame impala", "album"),
            ("blonde by frank ocean", "album"),
            ("thriller by michael jackson", "ask"),
            ("dracula by tame impala", "song"),
            ("look at me by xxxtentacion", "song"),
            ("the less i know the better by tame impala", "song"),
        ] {
            let request = crate::parser::typed_request(said).unwrap();
            let album_query = query(&request);
            let timing = crate::timing::Timing::default();
            let (songs, albums) = tokio::join!(
                crate::search::resolve_request(
                    &request,
                    &catalog,
                    None,
                    None,
                    serenity::model::id::GuildId::new(1),
                    &timing,
                ),
                catalog.albums(&album_query),
            );
            let settled = settle(&request.query, &songs, find(&request.query, &albums, false));
            let got = match &settled {
                Settle::Song => "song",
                Settle::Album(_) => "album",
                Settle::Ask { .. } => "ask",
            };
            println!("{said:45} -> {settled:?}");
            assert_eq!(got, expected, "{said}: {settled:?} (songs: {songs:?})");
        }

        // Saying "album" goes straight to the albums.
        let request = crate::parser::typed_request("thriller by michael jackson").unwrap();
        let albums = catalog.albums(&query(&request)).await;
        assert!(
            matches!(
                find(&request.query, &albums, true),
                Found::One(ref album) if album.title == "Thriller" && album.artist == "Michael Jackson"
            ),
            "{albums:?}"
        );
    }
}
