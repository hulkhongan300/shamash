use super::*;

#[test]
fn video_links_of_every_shape_are_read() {
    for link in [
        "https://www.youtube.com/watch?v=cuMuMnCRfqk",
        "https://youtube.com/watch?v=cuMuMnCRfqk&t=10",
        "https://m.youtube.com/watch?v=cuMuMnCRfqk",
        "https://music.youtube.com/watch?v=cuMuMnCRfqk&si=abc",
        "https://youtu.be/cuMuMnCRfqk?si=abc",
        "https://www.youtube.com/shorts/cuMuMnCRfqk",
        "https://www.youtube.com/live/cuMuMnCRfqk",
        "  https://www.youtube.com/watch?v=cuMuMnCRfqk  ",
    ] {
        assert_eq!(video_id(link).as_deref(), Some("cuMuMnCRfqk"), "{link}");
    }
}

/// The bug this replaced: any single word was taken for a video id, so
/// `/play dracula` tried to play a video called "dracula".
#[test]
fn a_bare_word_is_a_search_not_a_video() {
    for text in [
        "dracula",
        "cuMuMnCRfqk",
        "xxxtentacion",
        "dracula tame impala",
    ] {
        assert_eq!(video_id(text), None, "{text}");
        assert_eq!(playlist_url(text), None, "{text}");
    }
}

#[test]
fn other_sites_and_broken_ids_are_not_videos() {
    assert_eq!(video_id("https://example.com/watch?v=cuMuMnCRfqk"), None);
    assert_eq!(video_id("https://www.youtube.com/watch?v=short"), None);
    assert_eq!(video_id("https://www.youtube.com/@TameImpala"), None);
}

#[test]
fn playlist_and_album_links_are_read() {
    let album = "https://www.youtube.com/playlist?list=OLAK5uy_kW9hN-oBmekJ06jhhfStpwRd5pcRKIztY";
    assert_eq!(playlist_url(album).as_deref(), Some(album));
    assert_eq!(
        playlist_url("https://music.youtube.com/playlist?list=PLabc123XYZ&si=x").as_deref(),
        Some("https://www.youtube.com/playlist?list=PLabc123XYZ")
    );
    assert_eq!(
        playlist_url("https://music.youtube.com/browse/MPREb_abc123DEF").as_deref(),
        Some("https://music.youtube.com/browse/MPREb_abc123DEF")
    );
}

/// Sharing one song from a playlist shares a link with both ids. It is
/// that song, not the playlist.
#[test]
fn a_video_inside_a_playlist_is_that_video() {
    let link = "https://www.youtube.com/watch?v=cuMuMnCRfqk&list=OLAK5uy_kW9hN-oBmekJ06jhhfStpwRd5pcRKIztY";
    assert_eq!(playlist_url(link), None);
    assert_eq!(video_id(link).as_deref(), Some("cuMuMnCRfqk"));
}

#[test]
fn a_listing_keeps_the_order_and_drops_what_cannot_play() {
    let json = serde_json::json!({
        "title": "Album - 17",
        "entries": [
            {"id": "pG-tbrzlMVU", "title": "The Explanation", "duration": 51.0},
            {"id": "AAAAAAAAAAA", "title": "[Private video]"},
            {"id": "C1D3G2VGQ_8", "title": "Jocelyn Flores", "duration": 120.0,
             "channel": "XXXTENTACION"},
            {"title": "no id"},
        ]
    });
    let playlist = parse_listing(&json).unwrap();
    assert_eq!(playlist.title, "Album - 17");
    assert_eq!(playlist.unavailable, 2);
    let titles: Vec<_> = playlist.tracks.iter().map(|t| t.title.as_str()).collect();
    assert_eq!(titles, ["The Explanation", "Jocelyn Flores"]);
    assert_eq!(playlist.tracks[1].channel.as_deref(), Some("XXXTENTACION"));
}

#[test]
fn a_single_video_is_not_a_playlist() {
    let json = serde_json::json!({"id": "cuMuMnCRfqk", "title": "Dracula"});
    assert!(parse_listing(&json).is_err());
}

#[test]
fn a_long_playlist_is_cut_to_the_room_left() {
    assert_eq!(fitting(11, 0, 20), 11);
    assert_eq!(fitting(11, 15, 20), 5);
    assert_eq!(fitting(50, 0, 20), 20);
    assert_eq!(fitting(11, 20, 20), 0);
    assert_eq!(fitting(11, 25, 20), 0);
}

/// Lists the album from the issue. Needs network and yt-dlp.
#[tokio::test]
#[ignore = "needs network and yt-dlp"]
async fn the_live_album_lists_in_order() {
    let url = playlist_url(
        "https://www.youtube.com/playlist?list=OLAK5uy_kW9hN-oBmekJ06jhhfStpwRd5pcRKIztY",
    )
    .unwrap();
    let playlist = list(&url).await.expect("listing");
    assert_eq!(playlist.tracks.len(), 11, "{playlist:?}");
    assert_eq!(playlist.tracks[0].title, "The Explanation");
    assert_eq!(playlist.tracks[10].title, "Ayala (Outro)");
}
