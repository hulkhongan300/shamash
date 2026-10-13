//! Offline matching of a spoken request against catalog hits.
//!
//! Pure functions, no network: everything here is unit-testable with
//! hand-written hits, and the whole point of the module is that deciding which
//! song was asked for is a *decision*, not a side effect of which provider
//! answered.
//!
//! The two ideas that do the work:
//!
//! - **A window, not a whole string.** The request "drakula tame impala" names
//!   the artist after the title, "tame impala drakula" names it before, and
//!   "hoolig by bts" has a filler word in the middle. Comparing the request to
//!   a title as one string scores all three badly. So the title is scored
//!   against the best *window* of the spoken words, and separately against the
//!   artist's.
//! - **Leftover words count against a match.** "Lover" and "Love Story" are one
//!   letter apart, and a whole-string similarity cannot tell them apart. But the
//!   request said "story" and nothing in "Lover" explains it, so the score has to
//!   fall. This is what separates the two, and it is why the title — not the
//!   artist — has to clear a gate on its own.
//!
//! Phonetic matching is the third tool: "billy eyelash" has to reach Billie
//! Eilish. Double Metaphone is built for English names and does nothing useful
//! on Korean titles, so the edit-distance path still carries those.

use rphonetic::{DoubleMetaphone, Encoder};
use strsim::{jaro_winkler, normalized_damerau_levenshtein as ndl};

/// A title must explain the request at least this well to be considered at all.
///
/// A gate on the title alone, deliberately. The old scorer let the artist be a
/// free pass: 0.7 title + 0.3 artist meant "right artist, wrong title" cleared
/// the bar, which is how "sweet disposition by temper trap" came to answer
/// with *Science of Fear*.
pub const TITLE_GATE: f32 = 0.65;
/// Combined (0.8 title + 0.2 artist) score needed to play without asking.
pub const ACCEPT: f32 = 0.72;
/// The winner must beat the best *different song* by this much, or we ask.
pub const MARGIN: f32 = 0.10;
/// Same title by another artist within this gap of the winner => ask which artist.
pub const ARTIST_GAP: f32 = 0.06;

/// Lowercase, drop decorations, drop release-noise words.
///
/// A bracketed span *after* the title has started is a decoration -- a feature
/// credit, a remaster year, a "Part 2" marker -- and is dropped. A bracket that
/// *opens* the string is not: catalogs write "(I Can't Get No) Satisfaction" and
/// "(How Does It Feel) Pios Bolly", and treating the prefix as an aside would
/// reduce the first to the bare word "satisfaction", which matches every song
/// called anything. Dropping it is not the conservative choice here; it is the
/// one that loses the title.
pub fn normalize(text: &str) -> String {
    let mut kept = String::with_capacity(text.len());
    let mut started = false;
    let mut aside = false;
    for c in text.to_lowercase().chars() {
        match c {
            '(' | '[' => aside = started,
            ')' | ']' => aside = false,
            _ if aside => {}
            c if c.is_alphanumeric() => {
                started = true;
                kept.push(c);
            }
            _ => kept.push(' '),
        }
    }
    noise_words(&kept).join(" ")
}

/// The words of a normalized string, with the release noise removed.
fn noise_words(normalized: &str) -> Vec<&str> {
    normalized
        .split_whitespace()
        .filter(|w| {
            !matches!(
                *w,
                "feat"
                    | "ft"
                    | "featuring"
                    | "remaster"
                    | "remastered"
                    | "version"
                    | "single"
                    | "album"
                    | "explicit"
                    | "edit"
                    | "mono"
                    | "stereo"
                    | "bonus"
                    | "deluxe"
                    | "anniversary"
            )
        })
        .collect()
}

/// Words that carry no identity in a spoken request.
///
/// These are excluded from the "unexplained" count, so a request padded with
/// them is not penalised for a title that does not mention them. "play" is in
/// the list because the parser normally strips it, but a paused command arrives
/// as the trailing fragment alone and can still carry one, and counting it left
/// every content word in "please play hello" unexplained.
fn is_filler(w: &str) -> bool {
    matches!(
        w,
        "by" | "please" | "play" | "song" | "hey" | "ok" | "okay" | "some" | "something"
    )
}

/// The phonetic key for a normalized string: one Double Metaphone code per word.
///
/// Only ASCII letters are encoded. Double Metaphone is built for English names
/// and has nothing to say about Hangul or accented Latin, but it does not
/// decline gracefully: `rphonetic` slices the input on byte boundaries that are
/// not character boundaries and panics on "Échame". Any title the bot will
/// actually meet — "Échame La Suya", "나비잠" — would take the whole process
/// down. So non-ASCII words contribute nothing to the key, which leaves the
/// phonetic floor silent for them and the edit-distance path carrying them.
fn phonetic_key(s: &str) -> String {
    let dm = DoubleMetaphone::default();
    s.split_whitespace()
        .filter(|word| word.chars().all(|c| c.is_ascii_alphabetic()))
        .map(|word| dm.encode(word))
        .collect()
}

/// Similarity of two *normalized* strings, 0..=1.
///
/// Length-aware edit similarity (spaces ignored, so "wonder wall" ==
/// "wonderwall"), a Jaro-Winkler nudge that cannot exceed what the edit
/// similarity supports (stops the Winkler prefix bonus from rating "hello"
/// ~ "hello brother"), and a phonetic floor.
pub fn sim(a: &str, b: &str) -> f32 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let (an, bn) = (a.replace(' ', ""), b.replace(' ', ""));
    let edit = (ndl(a, b) as f32).max(ndl(&an, &bn) as f32);
    let jw = jaro_winkler(&an, &bn) as f32;
    let blend = 0.6 * edit + 0.4 * jw * edit.max(0.5);
    let (ka, kb) = (phonetic_key(a), phonetic_key(b));
    let phonetic = if ka.len() >= 3 && ka == kb { 0.92 } else { 0.0 };
    blend.max(edit).max(phonetic)
}

/// Best-matching window of the transcript tokens for `target`: (score, start, len).
fn best_window(tokens: &[String], target: &str) -> (f32, usize, usize) {
    let n = target.split_whitespace().count();
    let mut best = (0.0f32, 0usize, 0usize);
    for len in n.saturating_sub(1).max(1)..=n + 1 {
        if len > tokens.len() {
            break;
        }
        for start in 0..=tokens.len() - len {
            let s = sim(target, &tokens[start..start + len].join(" "));
            if s > best.0 {
                best = (s, start, len);
            }
        }
    }
    best
}

/// How one catalog entry scored against the transcript.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Score {
    /// The title, discounted for spoken words it does not explain.
    pub title: f32,
    /// The artist.
    pub artist: f32,
    /// What [`decide`] sorts on: 0.8 title + 0.2 artist.
    pub combined: f32,
}

/// Scores one catalog (artist, title) against the transcript.
///
/// The title score is cut by the share of spoken content words that neither the
/// title nor the artist explains. That is what rejects "Lover" for "love story":
/// one letter apart, but "story" is left over.
pub fn score(transcript: &str, artist: &str, title: &str) -> Score {
    let tokens: Vec<String> = normalize(transcript)
        .split_whitespace()
        .map(String::from)
        .collect();
    let (title, artist) = (normalize(title), normalize(artist));
    let (ts, t0, tl) = best_window(&tokens, &title);
    let (a_s, a0, al) = best_window(&tokens, &artist);

    let mut explained = vec![false; tokens.len()];
    if ts >= 0.6 {
        explained[t0..t0 + tl].fill(true);
    }
    if a_s >= 0.75 {
        explained[a0..a0 + al].fill(true);
    }
    let content: Vec<usize> = (0..tokens.len())
        .filter(|&i| !is_filler(&tokens[i]))
        .collect();
    let unexplained =
        content.iter().filter(|&&i| !explained[i]).count() as f32 / content.len().max(1) as f32;

    let title_score = (ts * (1.0 - 0.8 * unexplained)).clamp(0.0, 1.0);
    Score {
        title: title_score,
        artist: a_s,
        combined: 0.8 * title_score + 0.2 * a_s,
    }
}

/// What the matcher concluded.
#[derive(Debug, PartialEq)]
pub enum Verdict<'a, T> {
    /// Safe to play without asking.
    Confident(&'a T),
    /// Plausible but not decidable: ask the user (best first, at most 3).
    Ambiguous(Vec<&'a T>),
    /// Nothing believable.
    NoMatch,
}

/// Decides between hits. `key` returns (artist, title). Hits are de-duplicated on
/// normalized (artist, title), so "Song (Live)" and "Song" are one song.
///
/// Four conditions for [`Verdict::Confident`], each of which exists because a
/// real request broke without it:
/// - the **title** clears [`TITLE_GATE`] on its own, so the right artist with
///   the wrong title can never win;
/// - the combined score clears [`ACCEPT`];
/// - the winner beats the best *different song* by [`MARGIN`], or is nearly
///   perfect on its own;
/// - no other artist has the same title within [`ARTIST_GAP`], because then the
///   request did not say enough to tell them apart and guessing is a coin toss.
pub fn decide<'a, T>(
    transcript: &str,
    hits: &'a [T],
    key: impl Fn(&T) -> (&str, &str),
) -> Verdict<'a, T> {
    let mut seen = std::collections::HashSet::new();
    let mut scored: Vec<(Score, &T, String, String)> = Vec::new();
    for h in hits {
        let (a, t) = key(h);
        let (na, nt) = (normalize(a), normalize(t));
        if nt.is_empty() || !seen.insert((na.clone(), nt.clone())) {
            continue;
        }
        scored.push((score(transcript, a, t), h, na, nt));
    }
    scored.sort_by(|x, y| y.0.combined.total_cmp(&x.0.combined));
    let Some(top) = scored.first() else {
        return Verdict::NoMatch;
    };
    if top.0.title < TITLE_GATE || top.0.combined < ACCEPT {
        return Verdict::NoMatch;
    }

    let rival_title = scored
        .iter()
        .skip(1)
        .filter(|s| s.3 != top.3)
        .map(|s| s.0.combined)
        .fold(0.0f32, f32::max);
    let same_title_other_artist: Vec<&T> = scored
        .iter()
        .skip(1)
        .filter(|s| s.3 == top.3 && s.2 != top.2 && top.0.combined - s.0.combined < ARTIST_GAP)
        .map(|s| s.1)
        .collect();

    let clear = top.0.combined - rival_title >= MARGIN || top.0.combined >= 0.95;
    if clear && same_title_other_artist.is_empty() {
        return Verdict::Confident(top.1);
    }
    let mut options = vec![top.1];
    options.extend(same_title_other_artist);
    options.extend(
        scored
            .iter()
            .skip(1)
            .filter(|s| s.0.title >= TITLE_GATE)
            .map(|s| s.1),
    );
    options.dedup_by(|a, b| std::ptr::eq(*a, *b));
    options.truncate(3);
    Verdict::Ambiguous(options)
}

#[cfg(test)]
mod tests {
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

    /// The seed benchmark from the design note, plus the cases that broke the
    /// previous scorer.
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

    /// The true song is among the hits: it must be played or offered, never
    /// replaced.
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

    /// The true song is NOT among the hits: nothing may be confidently played.
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

    /// "Stand By Me" must not be read as title "stand" by artist "me".
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

    /// The old scorer's worst answers, one per mechanism. Each was a confident
    /// wrong play before; each is asserted here so it cannot come back.
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

        // Right artist, one word different title.
        let hits = [("Adele", "Hello Brother"), ("Adele", "Someone Like You")];
        assert_eq!(decide("hello by adele", &hits, key), Verdict::NoMatch);

        // A wrong song by the right artist that the old scorer rated 0.630,
        // just over its 0.62 bar.
        let hits = [
            ("The Temper Trap", "Fader"),
            ("The Temper Trap", "Science of Fear"),
        ];
        assert_eq!(
            decide("sweet disposition by temper trap", &hits, key),
            Verdict::NoMatch
        );
    }

    /// "Lover" and "Love Story" are one letter apart. What separates them is
    /// that nothing in "Lover" explains the word "story".
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

    /// The artist-first order is as ordinary as the title-first one, and used
    /// to be worse: "drakula tame impala" names the artist *after* the title.
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

    /// Speech-to-text mangles names; the phonetic floor is what rescues those.
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

    /// A bare misheard title with no artist named is a *safe failure*, not a
    /// guess. "hoolig" is two edits from "hooligan", which is 0.75 -- below
    /// ACCEPT on its own -- so the request is offered or refused rather than
    /// played. Naming the artist is what carries it over, which is why the
    /// resolver also asks the catalog for the artist's tracks.
    #[test]
    fn a_misheard_title_with_no_artist_named_is_not_guessed() {
        let hits = [("BTS", "Hooligan"), ("BTS", "Dynamite")];
        assert_eq!(decide("hoolig", &hits, key), Verdict::NoMatch);
        assert!(score("hoolig", "BTS", "Hooligan").title >= TITLE_GATE);
    }

    /// A title that equals a common word is not distinctive, and the matcher
    /// must not treat "hey" or "okay" as content to be explained.
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

    /// Non-English titles work through the edit-distance path, which is what
    /// the phonetic floor cannot help with: Double Metaphone is built for
    /// English names and encodes Hangul to nothing useful.
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

        // A K-pop title spelled in Hangul on both sides.
        let hits = [("BTS", "나비잠"), ("BTS", "봄날"), ("BTS", "에 YOUNG")];
        assert!(matches!(
            decide("나비잠 by bts", &hits, key),
            Verdict::Confident(h) if h.1 == "나비잠"
        ));
    }

    /// Two recordings of one title are one song as far as matching goes.
    #[test]
    fn decorated_hits_collapse_to_one_song() {
        let hits = [
            ("Queen", "Don't Stop Me Now (Live)"),
            ("Queen", "Don't Stop Me Now"),
        ];
        // Which of the two survives does not matter -- they are the same song,
        // and the catalog's own order decides.
        match decide("dont stop me now queen", &hits, key) {
            Verdict::Confident(h) => assert_eq!(normalize(h.1), "don t stop me now"),
            other => panic!("{other:?}"),
        }
    }

    /// A request with nothing in it must not match anything.
    #[test]
    fn an_empty_transcript_matches_nothing() {
        let hits = [("Tame Impala", "Dracula")];
        assert_eq!(decide("", &hits, key), Verdict::NoMatch);
        assert_eq!(decide("   ", &hits, key), Verdict::NoMatch);
    }

    /// A hit with no usable title cannot be matched against anything.
    #[test]
    fn a_hit_with_no_title_is_ignored() {
        let hits = [("Tame Impala", "  "), ("Tame Impala", "Dracula")];
        assert!(matches!(
            decide("dracula tame impala", &hits, key),
            Verdict::Confident(h) if h.1 == "Dracula"
        ));
    }

    /// With nothing to choose from there is no match, not a panic.
    #[test]
    fn no_hits_is_no_match() {
        let hits: Vec<Hit> = Vec::new();
        assert_eq!(decide("dracula tame impala", &hits, key), Verdict::NoMatch);
    }

    /// The offered options are the best first and are capped, because the
    /// chooser has three buttons and no more.
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

    /// The thresholds are what the whole design rests on, so the numbers are
    /// pinned rather than left to drift with a retune.
    #[test]
    fn the_thresholds_are_the_documented_ones() {
        assert_eq!(TITLE_GATE, 0.65);
        assert_eq!(ACCEPT, 0.72);
        assert_eq!(MARGIN, 0.10);
        assert_eq!(ARTIST_GAP, 0.06);
    }
}
