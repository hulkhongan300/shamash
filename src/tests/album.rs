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
#[cfg(test)]
mod probe {
    #[tokio::test]
    #[ignore]
    async fn probe() {
        use crate::resolve::{Catalog, YtMusic};
        let catalog = Catalog::with_budget(vec![Box::new(YtMusic::new().unwrap())], 2_500);
        for said in [
            "skin by xxxtentacion",
            "skins from xxxtentacion",
            "skins by xxx tentacion",
            "skins by x x x tentacion",
            "the album skins from xxxtentacion",
            "skins by xxxtentacion.",
            "skins by triple x tentacion",
            "album skins xxxtentacion",
            "skins xxxtentacion",
            "the album skin by xxxtentacion",
        ] {
            let request = crate::parser::typed_request(said).unwrap();
            let named = crate::parser::album_request(&request);
            let r = named.as_ref().unwrap_or(&request);
            let q = crate::album::query(r);
            let albums = catalog.albums(&q).await;
            let timing = crate::timing::Timing::default();
            let songs = crate::search::resolve_request(
                r,
                &catalog,
                None,
                None,
                serenity::model::id::GuildId::new(1),
                &timing,
            )
            .await;
            println!(
                "\n== {said} | named={} query={:?} heard={:?}",
                named.is_some(),
                q,
                r.query
            );
            for a in albums.iter().take(3) {
                println!(
                    "   album {:?} {:?} {:?} score={:?}",
                    a.artist,
                    a.title,
                    a.kind,
                    crate::matcher::score(&r.query, &a.artist, &a.title)
                );
            }
            println!(
                "   find(false)={:?}\n   find(true)={:?}",
                crate::album::find(&r.query, &albums, false),
                crate::album::find(&r.query, &albums, true)
            );
            println!("   songs={songs:?}");
        }
    }
}
#[cfg(test)]
mod probe2 {
    #[tokio::test]
    #[ignore]
    async fn probe2() {
        for id in [
            "OLAK5uy_nPrb35riVC9upmdqXgRnFo41u6drBs8Ak",
            "OLAK5uy_kW9hN-oBmekJ06jhhfStpwRd5pcRKIztY",
        ] {
            let r = crate::playlist::list(&format!("https://music.youtube.com/playlist?list={id}"))
                .await;
            match r {
                Ok(p) => println!("{id}: {} tracks={}", p.title, p.tracks.len()),
                Err(e) => println!("{id}: ERR {e:#}"),
            }
        }
    }
}
