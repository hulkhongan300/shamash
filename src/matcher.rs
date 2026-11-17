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
    /// How well the request named the artist: zero unless it did.
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
    // Only a named artist should earn the artist term. A bare title still has
    // some edit-distance overlap with every artist name -- "biome fest" and
    // "Celestial Fury" share enough letters to score 0.27 -- and rewarding
    // that put a 4k-play cover above the official recording. The same 0.75
    // bar already decides whether the artist's words count as explained.
    let artist_score = if a_s >= 0.75 { a_s } else { 0.0 };
    Score {
        title: title_score,
        artist: artist_score,
        combined: 0.8 * title_score + 0.2 * artist_score,
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
#[path = "tests/matcher.rs"]
mod tests;
