//! The golden set: spoken requests, the song each one means, and the catalog
//! hits it has to choose from.
//!
//! This is the acceptance test the whole resolver design rests on. Every case
//! runs twice:
//!
//! - **present** — the true song is among the hits. It must be played, or
//!   offered as a choice that includes it. Never replaced by something else.
//! - **absent** — the true song has been removed. Nothing may be played at all.
//!
//! The absent mode is the one that matters. The old scorer confidently played a
//! wrong song in 11 of 19 such cases; the failure it produced is a bot that
//! plays the wrong music with no feedback, which is worse than one that says
//! it could not tell.
//!
//! Everything here is offline: the hits are hand-written stand-ins for what the
//! catalog providers really return, which is enough to test the *decision* and
//! not the HTTP.

use shamash::matcher::{self, Verdict};

/// A catalog hit, as `(artist, title)`.
type Hit = (&'static str, &'static str);

fn key(hit: &Hit) -> (&str, &str) {
    (hit.0, hit.1)
}

/// A spoken request, the song it means, and the other songs a provider would
/// plausibly have returned alongside it.
struct Case {
    said: &'static str,
    truth: Hit,
    others: Vec<Hit>,
}

fn c(said: &'static str, truth: Hit, others: Vec<Hit>) -> Case {
    Case {
        said,
        truth,
        others,
    }
}

/// Requests, real-looking mishearings, and the distractors around them.
///
/// Grouped by the way speech-to-text actually goes wrong: names mangled, words
/// dropped or added, "by" swallowed, titles that are ordinary words, the same
/// title by two artists, non-English titles, and the artist named before the
/// song rather than after.
fn golden() -> Vec<Case> {
    vec![
        // ---- misheard proper nouns, artist named after the title -------------
        c(
            "drakula by tame impala",
            ("Tame Impala", "Dracula"),
            vec![
                ("Tame Impala", "The Less I Know The Better"),
                ("Tame Impala", "Borderline"),
                ("Tame Impala", "Lost In Yesterday"),
            ],
        ),
        c(
            "hoolig by bts",
            ("BTS", "Hooligan"),
            vec![
                ("BTS", "Dynamite"),
                ("BTS", "Boy With Luv"),
                ("BTS", "Butter"),
            ],
        ),
        c(
            "dont stop me now queen",
            ("Queen", "Don't Stop Me Now"),
            vec![
                ("Queen", "Bohemian Rhapsody"),
                ("Queen", "Under Pressure"),
                ("Queen", "We Will Rock You"),
            ],
        ),
        c(
            "lose yourself eminem",
            ("Eminem", "Lose Yourself"),
            vec![
                ("Eminem", "Without Me"),
                ("Eminem", "Love The Way You Lie"),
                ("Eminem", "Mockingbird"),
            ],
        ),
        c(
            "the night we met by lord huron",
            ("Lord Huron", "The Night We Met"),
            vec![
                ("Lord Huron", "The Stranger"),
                ("Lord Huron", "Ends of the Earth"),
                ("Lord Huron", "Young Mountains"),
            ],
        ),
        c(
            "sicko mode travis scott",
            ("Travis Scott", "SICKO MODE"),
            vec![
                ("Travis Scott", "goosebumps"),
                ("Travis Scott", "HIGHEST IN THE ROOM"),
                ("Drake", "Nice For What"),
            ],
        ),
        c(
            "billy eyelash bad guy",
            ("Billie Eilish", "bad guy"),
            vec![
                ("Billie Eilish", "bury a friend"),
                ("Billie Eilish", "ocean eyes"),
                ("Billie Eilish", "Happier Than Ever"),
            ],
        ),
        // ---- the artist named first -----------------------------------------
        c(
            "tame impala dracula",
            ("Tame Impala", "Dracula"),
            vec![
                ("Tame Impala", "Borderline"),
                ("Tame Impala", "Let It Happen"),
            ],
        ),
        c(
            "eminem lose yourself",
            ("Eminem", "Lose Yourself"),
            vec![("Eminem", "Without Me"), ("Eminem", "Stan")],
        ),
        c(
            "queen dont stop me now",
            ("Queen", "Don't Stop Me Now"),
            vec![
                ("Queen", "Bohemian Rhapsody"),
                ("Queen", "Fat Bottomed Girls"),
            ],
        ),
        // ---- heavy mangles ---------------------------------------------------
        c(
            "wonder wall by oasis",
            ("Oasis", "Wonderwall"),
            vec![
                ("Oasis", "Don't Look Back In Anger"),
                ("Oasis", "Champagne Supernova"),
                ("Oasis", "Live Forever"),
            ],
        ),
        c(
            "sweet disposition by temper trap",
            ("The Temper Trap", "Sweet Disposition"),
            vec![
                ("The Temper Trap", "Fader"),
                ("The Temper Trap", "Science of Fear"),
                ("The Temper Trap", "Resistance"),
            ],
        ),
        c(
            "sweet dispozishun by temper trap",
            ("The Temper Trap", "Sweet Disposition"),
            vec![
                ("The Temper Trap", "Fader"),
                ("The Temper Trap", "Science of Fear"),
            ],
        ),
        c(
            "hello by adele",
            ("Adele", "Hello"),
            vec![
                ("Adele", "Hello Brother"),
                ("Adele", "Someone Like You"),
                ("Adele", "Rolling In The Deep"),
            ],
        ),
        c(
            "helo by adele",
            ("Adele", "Hello"),
            vec![("Adele", "Hello Brother"), ("Adele", "Someone Like You")],
        ),
        c(
            "shape of you by ed sheeran",
            ("Ed Sheeran", "Shape of You"),
            vec![
                ("Ed Sheeran", "Perfect"),
                ("Ed Sheeran", "Shivers"),
                ("Ed Sheeran", "Castle on the Hill"),
            ],
        ),
        c(
            "hey jude by the beatles",
            ("The Beatles", "Hey Jude"),
            vec![
                ("The Beatles", "Hey Bulldog"),
                ("The Beatles", "Let It Be"),
                ("The Beatles", "Yesterday"),
            ],
        ),
        c(
            "take on me by a ha",
            ("a-ha", "Take On Me"),
            vec![
                ("a-ha", "The Sun Always Shines on T.V."),
                ("a-ha", "Hunting High and Low"),
                ("a-ha", "Crying in the Rain"),
            ],
        ),
        c(
            "love story by taylor swift",
            ("Taylor Swift", "Love Story"),
            vec![
                ("Taylor Swift", "Love Me Like You Do"),
                ("Taylor Swift", "Lover"),
                ("Taylor Swift", "Blank Space"),
            ],
        ),
        c(
            "bohemian rhapsody by queen",
            ("Queen", "Bohemian Rhapsody"),
            vec![
                ("Queen", "We Will Rock You"),
                ("Queen", "Another One Bites The Dust"),
                ("Freddie Mercury", "Living On A Prayer"),
            ],
        ),
        c(
            "smells like teen spirit by nirvana",
            ("Nirvana", "Smells Like Teen Spirit"),
            vec![
                ("Nirvana", "Come As You Are"),
                ("Nirvana", "Lithium"),
                ("Pearl Jam", "Alive"),
            ],
        ),
        c(
            "mr brightside by the killers",
            ("The Killers", "Mr. Brightside"),
            vec![
                ("The Killers", "Somebody Told Me"),
                ("The Killers", "When You Were Young"),
                ("Oasis", "Go Let It Out"),
            ],
        ),
        c(
            "the chain by fleetwood mac",
            ("Fleetwood Mac", "The Chain"),
            vec![
                ("Fleetwood Mac", "Dreams"),
                ("Fleetwood Mac", "Go Your Own Way"),
                ("Fleetwood Mac", "Landslide"),
            ],
        ),
        // ---- missing and extra words ----------------------------------------
        c(
            "dark side of the moon pink floyd",
            ("Pink Floyd", "The Dark Side of the Moon"),
            vec![
                ("Pink Floyd", "Money"),
                ("Pink Floyd", "Wish You Were Here"),
            ],
        ),
        c(
            "dark side moon pink floyd",
            ("Pink Floyd", "The Dark Side of the Moon"),
            vec![("Pink Floyd", "Money"), ("Pink Floyd", "High Hopes")],
        ),
        c(
            "despacito by justin bieber",
            ("Justin Bieber", "Despacito"),
            vec![
                ("Luis Fonsi", "Despacito"),
                ("Justin Bieber", "Sorry"),
                ("Justin Bieber", "Love Yourself"),
            ],
        ),
        c(
            "seven rings by ariana grande",
            ("Ariana Grande", "7 rings"),
            vec![
                ("Ariana Grande", "thank u, next"),
                ("Ariana Grande", "positions"),
            ],
        ),
        c(
            "twenty two by taylor swift",
            ("Taylor Swift", "22"),
            vec![
                ("Taylor Swift", "Blame It On the Rain"),
                ("Taylor Swift", "Exile"),
            ],
        ),
        c(
            "twenty two savage",
            ("Twenty One Pilots", "Stressed Out"),
            vec![("Twenty One Pilots", "Ride"), ("Kendrick Lamar", "HUMBLE.")],
        ),
        c(
            "roar by katy perry",
            ("Katy Perry", "Roar"),
            vec![("Katy Perry", "Firework"), ("Katy Perry", "Teenage Dream")],
        ),
        c(
            "radioactive imagine dragons",
            ("Imagine Dragons", "Radioactive"),
            vec![
                ("Imagine Dragons", "Demons"),
                ("Imagine Dragons", "Believer"),
                ("Imagine Dragons", "Thunder"),
            ],
        ),
        c(
            "believer imagine dragons",
            ("Imagine Dragons", "Believer"),
            vec![
                ("Imagine Dragons", "Radioactive"),
                ("Imagine Dragons", "Demons"),
            ],
        ),
        // ---- "by" inside the title ------------------------------------------
        c(
            "stand by me by ben e king",
            ("Ben E. King", "Stand By Me"),
            vec![
                ("Ben E. King", "Don't Play That Song"),
                ("Ben E. King", "Love Is a Game"),
            ],
        ),
        c(
            "take me by the hand by jimi hendrix",
            ("The Jimi Hendrix Experience", "Take Me By The Hand"),
            vec![
                ("The Jimi Hendrix Experience", "Hey Joe"),
                ("The Jimi Hendrix Experience", "All Along The Watchtower"),
            ],
        ),
        c(
            "by the way daft punk",
            ("Daft Punk", "By The Way"),
            vec![
                ("Daft Punk", "Get Lucky"),
                ("Daft Punk", "Around The World"),
            ],
        ),
        // ---- the same title by more than one artist -------------------------
        c(
            "stand by me by oasis",
            ("Oasis", "Stand By Me"),
            vec![("Ben E. King", "Stand By Me"), ("Oasis", "Wonderwall")],
        ),
        c(
            "hello by adele hello",
            ("Adele", "Hello"),
            vec![("Adele", "Hello Brother"), ("Lionel Richie", "Hello")],
        ),
        c(
            "gravity by daft punk",
            ("Daft Punk", "Gravity"),
            vec![("Daft Punk", "Get Lucky"), ("Sara Bareilles", "Gravity")],
        ),
        // ---- titles that are ordinary words ----------------------------------
        c(
            "the one and only by chris rea",
            ("Chris Rea", "The One And Only"),
            vec![
                ("Chris Rea", "Fool (If You Think It's Over)"),
                ("Chris Rea", "Somebody's Trying"),
            ],
        ),
        c(
            "hello hello by adele",
            ("Adele", "Hello"),
            vec![("Adele", "Hello Brother"), ("Adele", "Someone Like You")],
        ),
        c(
            "cry me a river by justin timberlake",
            ("Justin Timberlake", "Cry Me a River"),
            vec![
                ("Justin Timberlake", "SexyBack"),
                ("Justin Timberlake", "Mirrors"),
            ],
        ),
        // ---- feat. decorations and version noise -----------------------------
        c(
            "old town road by lil nas x",
            ("Lil Nas X", "Old Town Road"),
            vec![
                ("Lil Nas X", "Montero"),
                ("Billy Ray Cyrus", "Old Town Road"),
                ("Lil Nas X", "Industry Baby"),
            ],
        ),
        c(
            "despacito remix by luisa fonsi",
            ("Luis Fonsi", "Despacito"),
            vec![
                ("Justin Bieber", "Despacito"),
                ("Luis Fonsi", "Échame La Suya"),
            ],
        ),
        c(
            "uptown funk by mark ronson",
            ("Mark Ronson", "Uptown Funk"),
            vec![
                ("Mark Ronson", "Uptown Funk"),
                ("Bruno Mars", "Uptown Funk"),
                ("Mark Ronson", "Valerie"),
            ],
        ),
        // ---- non-English and K-pop -------------------------------------------
        c(
            "나비잠 by bts",
            ("BTS", "나비잠"),
            vec![("BTS", "봄날"), ("BTS", "에 YOUNG"), ("EXO", "Love Shot")],
        ),
        c(
            "dynamite by bts",
            ("BTS", "Dynamite"),
            vec![
                ("BTS", "Butter"),
                ("BTS", "Boy With Luv"),
                ("BLACKPINK", "Dynamite"),
            ],
        ),
        c(
            "밤편지 by 아이유",
            ("아이유", "밤편지"),
            vec![("아이유", "너의 이름"), ("아이유", "Love wins all")],
        ),
        // ---- requests with filler around them --------------------------------
        // The parser has already removed the wake word and "play" by this
        // point, so these are what the matcher actually sees.
        c(
            "hey shape of you please",
            ("Ed Sheeran", "Shape of You"),
            vec![("Ed Sheeran", "Perfect"), ("Ed Sheeran", "Galway Girl")],
        ),
        c(
            "ok despacito please",
            ("Justin Bieber", "Despacito"),
            vec![("Luis Fonsi", "Despacito"), ("Justin Bieber", "Sorry")],
        ),
        // A one-word title two artists share. Nothing in the request separates
        // them, so the matcher must offer both rather than coin-flip.
        c(
            "hello",
            ("Adele", "Hello"),
            vec![
                ("Adele", "Hello Brother"),
                ("Lionel Richie", "Hello"),
                ("Adele", "Someone Like You"),
            ],
        ),
        c(
            "please play hello",
            ("Adele", "Hello"),
            vec![
                ("Adele", "Hello Brother"),
                ("Lionel Richie", "Hello"),
                ("Adele", "Someone Like You"),
            ],
        ),
        c(
            "something by adele",
            ("Adele", "Rolling In The Deep"),
            vec![("Adele", "Hello"), ("Adele", "Someone Like You")],
        ),
        // ---- long titles -----------------------------------------------------
        c(
            "a change is gonna come by sam cooke",
            ("Sam Cooke", "A Change Is Gonna Come"),
            vec![
                ("Sam Cooke", "Cupid"),
                ("Sam Cooke", "Bring It On Home To Me"),
                ("Otis Redding", "Sittin' On The Dock Of The Bay"),
            ],
        ),
        c(
            "rolling stones i cant get no satisfaction",
            ("The Rolling Stones", "(I Can't Get No) Satisfaction"),
            vec![
                ("The Rolling Stones", "Paint It, Black"),
                ("The Rolling Stones", "Jumpin' Jack Flash"),
            ],
        ),
        c(
            "i cant get no satisfaction by the rolling stones",
            ("The Rolling Stones", "(I Can't Get No) Satisfaction"),
            vec![
                ("The Rolling Stones", "Paint It, Black"),
                ("The Rolling Stones", "Jumpin' Jack Flash"),
            ],
        ),
        c(
            "the sound of silence by simon and garfunkel",
            ("Simon & Garfunkel", "The Sound of Silence"),
            vec![
                ("Simon & Garfunkel", "Bridge Over Troubled Water"),
                ("Disturbed", "The Sound of Silence"),
            ],
        ),
        c(
            "midnight city by m83",
            ("M83", "Midnight City"),
            vec![
                ("M83", "Wait"),
                ("M83", "Outro"),
                ("Tame Impala", "The Less I Know The Better"),
            ],
        ),
        c(
            "electric feel by mgmt",
            ("MGMT", "Electric Feel"),
            vec![("MGMT", "Kids"), ("MGMT", "Time to Pretend")],
        ),
        c(
            "take five by dave brubeck",
            ("Dave Brubeck", "Take Five"),
            vec![
                ("Dave Brubeck", "Unsquare Dance"),
                ("Dave Brubeck", "Blue Rondo A la Turk"),
            ],
        ),
        c(
            "goodbye yellow brick road by elton john",
            ("Elton John", "Goodbye Yellow Brick Road"),
            vec![
                ("Elton John", "Rocket Man"),
                ("Elton John", "Your Song"),
                ("Toto", "Bohemian Rhapsody"),
            ],
        ),
        c(
            "purple rain by prince",
            ("Prince", "Purple Rain"),
            vec![
                ("Prince", "Kiss"),
                ("Prince", "1999"),
                ("Deep Purple", "Smoke On The Water"),
            ],
        ),
        c(
            "lose control by teddy swims",
            ("Teddy Swims", "Lose Control"),
            vec![
                ("Teddy Swims", "The Door"),
                ("Kendrick Lamar", "Lose Control"),
            ],
        ),
        c(
            "flowers by miley cyrus",
            ("Miley Cyrus", "Flowers"),
            vec![("Miley Cyrus", "Wrecking Ball"), ("Miley Cyrus", "Malibu")],
        ),
        c(
            "as it was by harry styles",
            ("Harry Styles", "As It Was"),
            vec![
                ("Harry Styles", "Watermelon Sugar"),
                ("Harry Styles", "Music for a Sushi Restaurant"),
            ],
        ),
        c(
            "watermelon sugar by harry styles",
            ("Harry Styles", "Watermelon Sugar"),
            vec![
                ("Harry Styles", "As It Was"),
                ("Harry Styles", "Late Night Talking"),
            ],
        ),
    ]
}

/// Requests the matcher is expected to refuse, with the reason each one is a
/// *safe* refusal rather than a gap.
///
/// Every one of these is a case where nothing in the request distinguishes the
/// song from its neighbours without a catalog lookup the matcher cannot do.
/// All are rescued downstream -- by the artist-only query, YouTube Music's
/// spell-correcting search, the LLM rewrite, or by offering the user a choice --
/// and none is played on a guess.
const EXPECTED_REFUSALS: [(&str, &str); 5] = [
    // "22" is two characters; the request said "twenty two". No string metric
    // bridges a number and its words, and the catalog search will not find
    // "22" from "twenty two" either.
    ("twenty two by taylor swift", "the title is digits"),
    // The transcript said "jimi hendrix"; the catalog says "The Jimi Hendrix
    // Experience".
    (
        "take me by the hand by jimi hendrix",
        "the artist was named by a different name",
    ),
    // Words were dropped from the middle of the title: "the dark side of the
    // moon" heard as "dark side moon". A matching window is contiguous and the
    // missing words are in the middle of it.
    (
        "dark side moon pink floyd",
        "words were dropped from inside the title",
    ),
    // "something" carries no identity, so there is nothing to match a specific
    // song against.
    ("something by adele", "the request named no song"),
    // "savage" heard for "Twenty One Pilots" is a heavy mishearing of a band
    // name, not a song title.
    ("twenty two savage", "the artist was misheard by half"),
];

/// The true song is present: it must be played or offered, never replaced.
#[test]
fn a_present_song_is_played_or_offered() {
    let mut refused = Vec::new();
    for case in golden() {
        let mut hits = vec![case.truth];
        hits.extend(case.others.iter().copied());
        match matcher::decide(case.said, &hits, key) {
            Verdict::Confident(hit) => assert_eq!(
                *hit, case.truth,
                "{:?}: played {:?} instead of {:?}",
                case.said, hit, case.truth
            ),
            Verdict::Ambiguous(options) => assert!(
                options.contains(&&case.truth),
                "{:?}: offered {:?}, which does not include {:?}",
                case.said,
                options,
                case.truth
            ),
            Verdict::NoMatch => refused.push(case.said),
        }
    }

    // Every refusal is one somebody predicted, with a reason.
    for said in &refused {
        assert!(
            EXPECTED_REFUSALS.iter().any(|(case, _)| case == said),
            "{said:?} should have resolved"
        );
    }
    // And the acceptance bar from the brief: at least 90% resolved.
    let cases = golden().len();
    let resolved = cases - refused.len();
    assert!(
        resolved * 100 >= cases * 90,
        "only {resolved} of {cases} present-mode cases resolved"
    );
    // Nothing predicted may have started resolving silently: that would be a
    // threshold change hiding inside a test edit.
    for (said, why) in EXPECTED_REFUSALS {
        assert!(
            refused.contains(&said),
            "{said:?} now resolves, so drop it from EXPECTED_REFUSALS ({why})"
        );
    }
}

/// The true song is absent: nothing may be played.
///
/// This is the test that matters. A wrong confident answer is worse than asking,
/// because the listener hears a song nobody asked for and has no way to tell it
/// from a right one.
#[test]
fn an_absent_song_is_never_played() {
    for case in golden() {
        let hits: Vec<Hit> = case
            .others
            .iter()
            .copied()
            .filter(|hit| matcher::normalize(hit.1) != matcher::normalize(case.truth.1))
            .collect();
        let verdict = matcher::decide(case.said, &hits, key);
        assert!(
            !matches!(verdict, Verdict::Confident(_)),
            "{:?}: played {:?} confidently when {:?} was not even in the hits",
            case.said,
            verdict,
            case.truth
        );
        // And anything offered must at least be a song that shares the title.
        if let Verdict::Ambiguous(options) = verdict {
            for option in options {
                assert!(
                    matcher::score(case.said, option.0, option.1).title >= matcher::TITLE_GATE,
                    "{:?}: offered {:?}, whose title does not match the request",
                    case.said,
                    option
                );
            }
        }
    }
}

/// The set itself has to be big enough to be worth having, and the two modes
/// have to agree about it, or this file is decoration.
#[test]
fn the_golden_set_is_worth_running() {
    let cases = golden();
    assert!(cases.len() >= 60, "only {} cases", cases.len());
    let mut distinct: Vec<&str> = cases.iter().map(|case| case.said).collect();
    distinct.sort_unstable();
    distinct.dedup();
    assert_eq!(
        distinct.len(),
        cases.len(),
        "two cases say the same thing, so one is not pulling its weight"
    );
    for case in &cases {
        assert!(
            !case.others.is_empty(),
            "{:?} has no distractors, so its absent mode is trivial",
            case.said
        );
    }
}

/// The same title by two artists, with no artist named, must ask rather than
/// pick. This is the README's "Look at Me" case, and the reason `decide` has an
/// artist gap at all.
#[test]
fn a_title_two_artists_share_is_offered_not_guessed() {
    let hits = [
        ("Naughty Boy", "Look at Me"),
        ("Tems", "Look at Me"),
        ("The Neighbourhood", "Look at Me"),
    ];
    match matcher::decide("look at me", &hits, key) {
        Verdict::Ambiguous(options) => assert!(options.len() >= 2, "{options:?}"),
        other => panic!("{other:?}"),
    }

    // Naming one artist settles it.
    let hits = [("Naughty Boy", "Look at Me"), ("Tems", "Look at Me")];
    assert!(matches!(
        matcher::decide("look at me by naughty boy", &hits, key),
        Verdict::Confident(hit) if hit.0 == "Naughty Boy"
    ));
}

/// A request that names the artist but a title the artist never recorded must
/// not be answered with that artist's other songs. This is the acceptance row
/// "true song missing, right artist named".
#[test]
fn a_missing_song_by_a_named_artist_is_not_swapped_for_another() {
    let hits = [
        ("The Temper Trap", "Fader"),
        ("The Temper Trap", "Science of Fear"),
        ("The Temper Trap", "Resistance"),
    ];
    assert_eq!(
        matcher::decide("sweet disposition by temper trap", &hits, key),
        Verdict::NoMatch
    );
}

/// Two recordings of one song are one song, so a "(Live)" hit must not make the
/// request ambiguous.
#[test]
fn versions_of_one_song_are_not_an_ambiguity() {
    let hits = [
        ("Queen", "Don't Stop Me Now"),
        ("Queen", "Don't Stop Me Now (Live at Wembley)"),
        ("Queen", "Don't Stop Me Now (2011 Remaster)"),
    ];
    assert!(matches!(
        matcher::decide("dont stop me now by queen", &hits, key),
        Verdict::Confident(_)
    ));
}
