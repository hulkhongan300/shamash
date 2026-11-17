//! Offline matching of a spoken request against catalog hits (pure, no network).
//!
//! Titles score against the best *window* of spoken words (artist order varies),
//! and words the hit leaves unexplained lower the score ("Lover" vs "love story").

use rphonetic::{DoubleMetaphone, Encoder};
use strsim::{jaro_winkler, normalized_damerau_levenshtein as ndl};

/// Minimum title score on its own, so the right artist can't carry a wrong title.
pub const TITLE_GATE: f32 = 0.65;
/// Combined (0.8 title + 0.2 artist) score needed to play without asking.
pub const ACCEPT: f32 = 0.72;
/// The winner must beat the best *different song* by this much, or we ask.
pub const MARGIN: f32 = 0.10;
/// Same title by another artist within this gap of the winner => ask which artist.
pub const ARTIST_GAP: f32 = 0.06;

/// Lowercase, drop decorations and release-noise words.
///
/// Brackets after the title starts are dropped; a leading one is kept, as in
/// "(I Can't Get No) Satisfaction".
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

/// [`normalize`] with numbers spelled out; scoring only, since memory keys stay normalized.
fn spoken(text: &str) -> String {
    normalize(text)
        .split(' ')
        .map(|w| spelled(w).unwrap_or_else(|| w.to_string()))
        .collect::<Vec<_>>()
        .join(" ")
}

/// A number up to 99 in words, since the phonetic floor only reads letters ("won" finds "One Dance").
fn spelled(word: &str) -> Option<String> {
    const ONES: [&str; 20] = [
        "zero",
        "one",
        "two",
        "three",
        "four",
        "five",
        "six",
        "seven",
        "eight",
        "nine",
        "ten",
        "eleven",
        "twelve",
        "thirteen",
        "fourteen",
        "fifteen",
        "sixteen",
        "seventeen",
        "eighteen",
        "nineteen",
    ];
    const TENS: [&str; 10] = [
        "", "", "twenty", "thirty", "forty", "fifty", "sixty", "seventy", "eighty", "ninety",
    ];
    if word.len() > 2 || !word.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let n: usize = word.parse().ok()?;
    Some(match n {
        0..20 => ONES[n].to_string(),
        _ if n.is_multiple_of(10) => TENS[n / 10].to_string(),
        _ => format!("{} {}", TENS[n / 10], ONES[n % 10]),
    })
}

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

/// Words excluded from the "unexplained" count. "play" survives the parser in paused commands.
fn is_filler(w: &str) -> bool {
    matches!(
        w,
        "by" | "from" | "please" | "play" | "song" | "hey" | "ok" | "okay" | "some" | "something"
    )
}

/// One Double Metaphone code per word, ASCII words only: `rphonetic` panics on "Échame".
fn phonetic_key(s: &str) -> String {
    let dm = DoubleMetaphone::default();
    s.split_whitespace()
        .filter(|word| word.chars().all(|c| c.is_ascii_alphabetic()))
        .map(|word| dm.encode(word))
        .collect()
}

/// Similarity of two *normalized* strings, 0..=1.
///
/// Jaro-Winkler is capped by edit similarity so "hello" doesn't match "hello brother".
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

/// Scores one catalog (artist, title); the title is cut by unexplained spoken words.
pub fn score(transcript: &str, artist: &str, title: &str) -> Score {
    let tokens: Vec<String> = spoken(transcript)
        .split_whitespace()
        .map(String::from)
        .collect();
    let (title, artist) = (spoken(title), spoken(artist));
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
    // Only a named artist scores; incidental letter overlap ranked covers above originals.
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

/// Decides between hits, de-duplicated on normalized `key` (artist, title).
///
/// Confident needs [`TITLE_GATE`], [`ACCEPT`], a [`MARGIN`] over other songs, and
/// no same title by another artist within [`ARTIST_GAP`].
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
