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
#[path = "tests/album.rs"]
mod tests;
