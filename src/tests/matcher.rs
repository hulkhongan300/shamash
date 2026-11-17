use super::*;

/// A catalog hit, as `(artist, title)`.
type Hit = (&'static str, &'static str);
fn key(h: &Hit) -> (&str, &str) {
    (h.0, h.1)
}

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

/// Seed benchmark plus the cases that broke the previous scorer.
fn golden() -> Vec<Case> {
    vec![
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
                ("Queen", "Don't Stop Me Now (Live)"),
                ("Queen", "Bohemian Rhapsody"),
                ("Queen", "Under Pressure"),
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
            "hello by adele",
            ("Adele", "Hello"),
            vec![("Adele", "Hello Brother"), ("Adele", "Someone Like You")],
        ),
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
            "lose yourself eminem",
            ("Eminem", "Lose Yourself"),
            vec![
                ("Eminem", "Without Me"),
                ("Eminem", "Love The Way You Lie"),
                ("Eminem", "Mockingbird"),
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
        c(
            "the night we met by lord huron",
            ("Lord Huron", "The Night We Met"),
            vec![
                ("Lord Huron", "The Stranger"),
                ("Lord Huron", "Ends of the Earth"),
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
            "shape of you",
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
    ]
}

#[test]
fn the_right_song_is_chosen_when_it_is_returned() {
    for case in golden() {
        let mut hits = vec![case.truth];
        hits.extend(case.others.clone());
        match decide(case.said, &hits, key) {
            Verdict::Confident(h) => assert_eq!(*h, case.truth, "{}", case.said),
            other => panic!("{:?} -> {:?}", case.said, other),
        }
    }
}

#[test]
fn a_wrong_song_is_never_confidently_played() {
    for case in golden() {
        let hits: Vec<Hit> = case
            .others
            .iter()
            .copied()
            .filter(|h| normalize(h.1) != normalize(case.truth.1))
            .collect();
        assert!(
            !matches!(decide(case.said, &hits, key), Verdict::Confident(_)),
            "{}",
            case.said
        );
    }
}

/// Letter overlap with a cover's artist must not rank it first; callers tie-break by plays.
#[test]
fn a_bare_title_does_not_favour_a_cover_by_artist_overlap() {
    let hits = [
        ("C418", "Biome Fest"),
        ("Celestial Fury", "Biome Fest (from \"Minecraft\")"),
    ];
    match decide("biome fest", &hits, key) {
        Verdict::Ambiguous(options) => {
            assert_eq!(*options[0], ("C418", "Biome Fest"), "{options:?}");
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn by_inside_a_title_is_not_a_split() {
    let hits = [
        ("Ben E. King", "Stand By Me"),
        ("Oasis", "Stand By Me"),
        ("Jack Johnson", "Better Together"),
    ];
    match decide("stand by me", &hits, key) {
        Verdict::Ambiguous(o) => assert_eq!(o.len(), 2, "two artists share the title"),
        other => panic!("{other:?}"),
    }
    let hits = [("Ben E. King", "Stand By Me"), ("Oasis", "Stand By Me")];
    assert!(
        matches!(decide("stand by me by ben e king", &hits, key), Verdict::Confident(h) if h.0 == "Ben E. King")
    );
}

#[test]
fn unrelated_titles_by_the_named_artist_are_no_match() {
    let hits = [
        ("The Temper Trap", "Fader"),
        ("The Temper Trap", "Science of Fear"),
    ];
    assert_eq!(
        decide("sweet disposition by temper trap", &hits, key),
        Verdict::NoMatch
    );
}

/// Regressions: each was a confident wrong play under the old scorer.
#[test]
fn the_old_scorers_wrong_answers_are_now_refused() {
    // Right artist, wrong title: the artist is a bonus, never a pass.
    let hits = [
        ("Taylor Swift", "Love Yourself"),
        ("Taylor Swift", "Lover"),
        ("Taylor Swift", "Blank Space"),
    ];
    assert_eq!(
        decide("love story by taylor swift", &hits, key),
        Verdict::NoMatch,
        "one shared word must not carry a wrong song"
    );

    let hits = [("Adele", "Hello Brother"), ("Adele", "Someone Like You")];
    assert_eq!(decide("hello by adele", &hits, key), Verdict::NoMatch);

    // The old scorer rated this 0.630, just over its 0.62 bar.
    let hits = [
        ("The Temper Trap", "Fader"),
        ("The Temper Trap", "Science of Fear"),
    ];
    assert_eq!(
        decide("sweet disposition by temper trap", &hits, key),
        Verdict::NoMatch
    );
}

#[test]
fn a_title_that_leaves_a_word_unexplained_is_discounted() {
    let heard = "love story by taylor swift";
    let right = score(heard, "Taylor Swift", "Love Story");
    let wrong = score(heard, "Taylor Swift", "Lover");
    assert!(right.title > wrong.title, "{right:?} vs {wrong:?}");
    assert!(
        wrong.title < TITLE_GATE,
        "the wrong one must not pass: {wrong:?}"
    );
}

#[test]
fn the_artist_may_come_before_or_after_the_title() {
    let hits = [("Tame Impala", "Dracula"), ("Tame Impala", "Borderline")];
    for said in [
        "drakula by tame impala",
        "tame impala dracula",
        "tame impala drakula please",
        "play drakula tame impala",
    ] {
        assert!(
            matches!(decide(said, &hits, key), Verdict::Confident(h) if h.1 == "Dracula"),
            "{said:?}"
        );
    }
}

#[test]
fn a_misheard_name_still_matches() {
    assert!(
        sim(
            normalize("billy eyelash").as_str(),
            normalize("billie eilish").as_str()
        ) > 0.9
    );
    for heard in [
        "hoolig by bts",
        "hoolegan by bts",
        "hooligan by bts",
        "bts hooligan",
    ] {
        let hits = [("BTS", "Hooligan"), ("BTS", "Dynamite")];
        assert!(
            matches!(decide(heard, &hits, key), Verdict::Confident(h) if h.1 == "Hooligan"),
            "{heard:?}"
        );
    }
}

/// "hoolig" alone scores below ACCEPT: refused, not guessed. Naming the artist carries it.
#[test]
fn a_misheard_title_with_no_artist_named_is_not_guessed() {
    let hits = [("BTS", "Hooligan"), ("BTS", "Dynamite")];
    assert_eq!(decide("hoolig", &hits, key), Verdict::NoMatch);
    assert!(score("hoolig", "BTS", "Hooligan").title >= TITLE_GATE);
}

#[test]
fn filler_words_are_not_content() {
    let hits = [
        ("The Police", "Every Breath You Take"),
        ("a-ha", "Take On Me"),
    ];
    let score = score(
        "hey play every breath you take",
        "The Police",
        "Every Breath You Take",
    );
    assert!(score.title > TITLE_GATE, "{score:?}");
    assert!(
        matches!(decide("hey play every breath you take", &hits, key), Verdict::Confident(h) if h.0 == "The Police")
    );
}

/// Hangul relies on edit distance; Double Metaphone ignores it.
#[test]
fn non_english_titles_still_match() {
    let hits = [
        ("아이유", "밤편지"),
        ("아이유", "너의 이름"),
        ("아이유", "Love wins all"),
    ];
    assert!(matches!(
        decide("밤편지 아이유", &hits, key),
        Verdict::Confident(h) if h.1 == "밤편지"
    ));

    let hits = [("BTS", "나비잠"), ("BTS", "봄날"), ("BTS", "에 YOUNG")];
    assert!(matches!(
        decide("나비잠 by bts", &hits, key),
        Verdict::Confident(h) if h.1 == "나비잠"
    ));
}

#[test]
fn decorated_hits_collapse_to_one_song() {
    let hits = [
        ("Queen", "Don't Stop Me Now (Live)"),
        ("Queen", "Don't Stop Me Now"),
    ];
    // Either survivor is fine; they are the same song.
    match decide("dont stop me now queen", &hits, key) {
        Verdict::Confident(h) => assert_eq!(normalize(h.1), "don t stop me now"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn an_empty_transcript_matches_nothing() {
    let hits = [("Tame Impala", "Dracula")];
    assert_eq!(decide("", &hits, key), Verdict::NoMatch);
    assert_eq!(decide("   ", &hits, key), Verdict::NoMatch);
}

#[test]
fn a_hit_with_no_title_is_ignored() {
    let hits = [("Tame Impala", "  "), ("Tame Impala", "Dracula")];
    assert!(matches!(
        decide("dracula tame impala", &hits, key),
        Verdict::Confident(h) if h.1 == "Dracula"
    ));
}

#[test]
fn no_hits_is_no_match() {
    let hits: Vec<Hit> = Vec::new();
    assert_eq!(decide("dracula tame impala", &hits, key), Verdict::NoMatch);
}

/// The chooser has three buttons.
#[test]
fn an_ambiguous_verdict_offers_at_most_three_best_first() {
    let hits = [
        ("Ben E. King", "Stand By Me"),
        ("Oasis", "Stand By Me"),
        ("A Ha", "Take On Me"),
        ("Tones And I", "Dance Monkey"),
    ];
    match decide("stand by me", &hits, key) {
        Verdict::Ambiguous(options) => {
            assert!(options.len() <= 3, "{options:?}");
            assert_eq!(*options[0], ("Ben E. King", "Stand By Me"));
        }
        other => panic!("{other:?}"),
    }
}

/// Pinned so a retune is deliberate.
#[test]
fn the_thresholds_are_the_documented_ones() {
    assert_eq!(TITLE_GATE, 0.65);
    assert_eq!(ACCEPT, 0.72);
    assert_eq!(MARGIN, 0.10);
    assert_eq!(ARTIST_GAP, 0.06);
}
