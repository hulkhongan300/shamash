//! Whether a spoken request was for a whole album, and which one.
//!
//! Saying "album" ([`crate::parser::album_request`]) searches only albums.
//! Otherwise song and album are searched together and [`settle`] decides:
//!
//! | the songs | the artist was named | what happens |
//! | --- | --- | --- |
//! | nothing matched | yes | the album plays |
//! | nothing matched | no | "Which one?", with the album on offer |
//! | several songs | either | "Which one?", the album first, then the songs |
//! | one song | yes | "Which one?": the song or the album |
//! | one song | no | the song plays |
//!
//! Unsaid, an album's title must match near-exactly, and singles never count
//! (YouTube Music lists most songs as their own single).

use crate::matcher::{self, Verdict};
use crate::parser::PlayRequest;
use crate::resolve::{CatalogAlbum, CatalogTrack, ReleaseKind};
use crate::search::Resolved;

/// Album search query: title and artist without the "by", as the song search asks.
pub fn query(request: &PlayRequest) -> String {
    match request.splits.first() {
        Some((title, artist)) => format!("{title} {artist}"),
        None => request.query.clone(),
    }
}

/// Title bar when "album" was not said; the matcher's 0.65 is too loose to interrupt a song.
const UNSAID_TITLE: f32 = 0.9;

/// Which releases match the words.
#[derive(Debug, Clone, PartialEq)]
pub enum Found {
    One(CatalogAlbum),
    Several(Vec<CatalogAlbum>),
    Nothing,
}

/// Which release `said` names; `named_as_album` admits singles and the matcher's usual bar.
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
        // Only ask between albums when "album" was said.
        Found::Several(_) if !named_as_album => Found::Nothing,
        found => found,
    }
}

/// What to do with a request that was searched for as a song and an album.
#[derive(Debug, Clone, PartialEq)]
pub enum Settle {
    /// Carry on as if no album had been looked for.
    Song,
    Album(CatalogAlbum),
    /// Ask, album first, then these songs.
    Ask {
        album: CatalogAlbum,
        songs: Vec<CatalogTrack>,
    },
}

/// Song or album; see the table in the module docs.
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
