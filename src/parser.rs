use std::collections::HashSet;

/// A music request extracted from a spoken command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlayRequest {
    /// The search query to send to the music source.
    pub query: String,
    /// The detected title, when the transcript had "play <title> by <artist>".
    pub title: Option<String>,
    /// The detected artist, when the transcript had "play <title> by <artist>".
    pub artist: Option<String>,
    /// Every "<title> by <artist>" reading, since "by" may belong to the title.
    pub splits: Vec<(String, String)>,
}

/// The verb a rewriter or an armed speaker may repeat before the request.
const PLAY_VERB: &str = "play";

/// Polite openers allowed between the wake word and the verb: "groovy, could you play".
const ASKS: &[&[&str]] = &[
    &["can", "you"],
    &["could", "you"],
    &["would", "you"],
    &["will", "you"],
];

/// Verbs that ask for a song after the wake word; the models often write "play" as "played".
///
/// Not "queue": the wake model runs it into the next word ("cubohemian rhapsody").
const VERBS: &[&[&str]] = &[&["play"], &["played"], &["put", "on"], &["throw", "on"]];

/// What follows "[could you] [please] <verb>" at the start of `words`, if a verb is there.
fn after_verb<'a>(words: &'a [&'a str]) -> Option<&'a [&'a str]> {
    let words = ASKS
        .iter()
        .find_map(|ask| words.strip_prefix(*ask))
        .unwrap_or(words);
    let words = words.strip_prefix(&["please"]).unwrap_or(words);
    VERBS.iter().find_map(|verb| words.strip_prefix(*verb))
}

/// Whether `heard` could be a speech model's spelling of the wake word `wake`.
///
/// They lose vowels and the first consonant ("rove", "Roovy" for "groovy") but
/// keep the rest, so the consonants are compared: all the same, or the same but
/// for the first. The word must also end alike, in a vowel or not, which keeps
/// "rob played" out. Under three consonants it must be exact, or "bot" would
/// wake on "but". `b_is_v` is for the wake model, which writes "ruby" for
/// "groovy"; from the request model, "Ruby" is somebody's name.
fn sounds_like(heard: &str, wake: &str, b_is_v: bool) -> bool {
    if heard == wake {
        return true;
    }
    let ends_in_vowel = |word: &str| word.ends_with(['a', 'e', 'i', 'o', 'u', 'y', 'w']);
    let (h, w) = (consonants(heard, b_is_v), consonants(wake, b_is_v));
    w.len() >= 3
        && ends_in_vowel(heard) == ends_in_vowel(wake)
        && (h == w || h[..] == w[1..] || (h.len() == w.len() && h[1..] == w[1..]))
}

/// A word's consonants, with letters that sound alike merged and repeats dropped.
fn consonants(word: &str, b_is_v: bool) -> Vec<char> {
    let mut out: Vec<char> = Vec::new();
    for c in word.chars() {
        let c = match c {
            'b' | 'p' if !b_is_v => 'b',
            'b' | 'f' | 'p' | 'v' => 'v',
            'c' | 'g' | 'j' | 'k' | 'q' => 'g',
            'd' | 't' => 't',
            's' | 'x' | 'z' => 's',
            'l' | 'm' | 'n' | 'r' => c,
            _ => continue,
        };
        if out.last() != Some(&c) {
            out.push(c);
        }
    }
    out
}

/// Spoken asides Whisper appends after the request ("look at me. come on").
const TRAILING_ASIDES: &[&str] = &["come on"];

/// Drops a trailing aside only at a word boundary ("welcome on" stays).
fn without_trailing_aside(text: &str) -> &str {
    let mut text = text.trim_end();
    loop {
        let mut stripped = None;
        for aside in TRAILING_ASIDES {
            if let Some(prefix) = text.strip_suffix(aside)
                && prefix.ends_with(' ')
            {
                let rest = prefix.trim_end();
                if !rest.is_empty() {
                    stripped = Some(rest);
                    break;
                }
            }
        }
        match stripped {
            Some(rest) => text = rest,
            None => return text,
        }
    }
}

/// Why a transcript did not yield a play request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParseMiss {
    /// No configured wake word was heard.
    NoWakeWord,
    /// The wake word was heard, but "play" never followed it.
    NoPlayVerb,
    /// "play" was heard with nothing to play after it.
    EmptyRequest,
}

impl std::fmt::Display for ParseMiss {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let reason = match self {
            Self::NoWakeWord => "no wake word heard",
            Self::NoPlayVerb => "no \"play\" after the wake word",
            Self::EmptyRequest => "nothing to play after \"play\"",
        };
        f.write_str(reason)
    }
}

/// Turns transcripts into [`PlayRequest`]s: "<wake word> [could you] [please] play <request>".
#[derive(Debug, Clone)]
pub struct CommandParser {
    wake_words: HashSet<String>,
}

impl CommandParser {
    pub fn new(wake_words: impl IntoIterator<Item = String>) -> Self {
        Self {
            wake_words: wake_words
                .into_iter()
                .map(|w| w.to_ascii_lowercase())
                .collect(),
        }
    }

    /// The wake words this parser accepts.
    pub fn wake_words(&self) -> &HashSet<String> {
        &self.wake_words
    }

    /// Whether `word` is one of the configured wake words, heard exactly.
    pub fn is_wake_word(&self, word: &str) -> bool {
        self.wake_words.contains(word)
    }

    /// Where the request starts, when `words[at]` sounds like a wake word and a
    /// verb follows it, perhaps asked politely: "groovy, could you please play".
    ///
    /// By sound, not spelling: spelled exactly, four in five requests were missed
    /// in tests with 40 voices, as both models drop or swap the "g" ("ruby
    /// played", "Roovy, play").
    fn request_start(&self, words: &[&str], at: usize, b_is_v: bool) -> Option<usize> {
        if !self
            .wake_words
            .iter()
            .any(|wake| sounds_like(words[at], wake, b_is_v))
        {
            return None;
        }
        after_verb(&words[at + 1..]).map(|request| words.len() - request.len())
    }

    /// The request in "<wake word> play <request>", or `None`.
    pub fn parse(&self, transcript: &str) -> Option<PlayRequest> {
        self.command(transcript).ok()
    }

    /// The request alone, with any wake word and "play" removed (rewriters keep them).
    pub fn strip_command(&self, transcript: &str) -> Option<String> {
        if let Ok(request) = self.command(transcript) {
            return Some(request.query);
        }
        let text = normalize(transcript);
        let mut words: &[&str] = &text
            .split(' ')
            .filter(|w| !w.is_empty())
            .collect::<Vec<_>>();
        if words.first().is_some_and(|w| self.is_wake_word(w)) {
            words = &words[1..];
        }
        if words.first() == Some(&PLAY_VERB) {
            words = &words[1..];
        }
        build_request(&words.join(" ")).map(|request| request.query)
    }

    /// Whether the wake model's window holds "<wake word> play", or another way to ask.
    pub fn wakes_on(&self, window: &str) -> bool {
        let text = normalize(window);
        let words: Vec<&str> = text.split(' ').filter(|w| !w.is_empty()).collect();
        (0..words.len()).any(|at| self.request_start(&words, at, true).is_some())
    }

    /// The words after the first "<wake word> play", or another way to ask, are the request.
    pub fn command(&self, transcript: &str) -> Result<PlayRequest, ParseMiss> {
        let text = normalize(transcript);
        let words: Vec<&str> = text.split(' ').filter(|w| !w.is_empty()).collect();
        let Some(start) = (0..words.len()).find_map(|at| self.request_start(&words, at, false))
        else {
            return Err(if words.iter().any(|w| self.is_wake_word(w)) {
                ParseMiss::NoPlayVerb
            } else {
                ParseMiss::NoWakeWord
            });
        };
        build_request(&words[start..].join(" ")).ok_or(ParseMiss::EmptyRequest)
    }

    /// The request after "groovy play" and a pause; a repeated wake word or "play" is dropped.
    pub fn armed_request(&self, transcript: &str) -> Option<PlayRequest> {
        let text = normalize(transcript);
        let words: Vec<&str> = text.split(' ').filter(|w| !w.is_empty()).collect();
        let skip = words
            .iter()
            .take_while(|w| self.is_wake_word(w) || **w == PLAY_VERB)
            .count();
        build_request(&words[skip..].join(" "))
    }

    /// Builds a request from text spoken after a "play" that was already heard.
    pub fn request_from_query(&self, text: &str) -> Option<PlayRequest> {
        typed_request(text)
    }
}

/// Parses a bare typed request, cleaned like a spoken one so song memory keys match.
pub fn typed_request(text: &str) -> Option<PlayRequest> {
    build_request(&normalize(text))
}

/// What follows an "and" that surely starts another song: "and then", "and also", "and play".
fn next_song<'a>(words: &'a [&'a str]) -> Option<&'a [&'a str]> {
    let linked = ["then", "also"]
        .iter()
        .find_map(|w| words.strip_prefix(&[*w]));
    after_verb(linked.unwrap_or(words)).or(linked)
}

/// The songs in "x and y": split at every "and" when `every`, else only where
/// another song surely follows ("and play y"), so "Me and Bobby McGee" stays whole.
pub fn songs(query: &str, every: bool) -> Vec<PlayRequest> {
    let words: Vec<&str> = query.split(' ').filter(|w| !w.is_empty()).collect();
    let mut parts = Vec::new();
    let (mut start, mut at) = (0, 0);
    while at < words.len() {
        let rest = &words[at + 1..];
        match next_song(rest).or(every.then_some(rest)) {
            Some(next) if words[at] == "and" => {
                parts.push(&words[start..at]);
                start = words.len() - next.len();
                at = start;
            }
            _ => at += 1,
        }
    }
    parts.push(&words[start..]);
    parts
        .iter()
        .filter_map(|part| build_request(&part.join(" ")))
        .collect()
}

/// Album lead-ins before the title, longest first.
const ALBUM_LEADS: &[&[&str]] = &[
    &["the", "full", "album"],
    &["the", "whole", "album"],
    &["the", "album"],
    &["full", "album"],
    &["whole", "album"],
    &["album"],
];

/// The album a request explicitly asks for ("the album 17", "the 17 album", "17 by x album").
pub fn album_request(request: &PlayRequest) -> Option<PlayRequest> {
    let words: Vec<&str> = request.query.split(' ').filter(|w| !w.is_empty()).collect();
    if let Some(lead) = ALBUM_LEADS.iter().find(|lead| words.starts_with(lead)) {
        let mut rest = &words[lead.len()..];
        // "the album version of Dracula" is a song.
        if rest.first() == Some(&"version") {
            return None;
        }
        if rest
            .first()
            .is_some_and(|w| matches!(*w, "called" | "named"))
        {
            rest = &rest[1..];
        }
        return build_request(&rest.join(" "));
    }
    let at = words.iter().rposition(|w| *w == "album")?;
    let title = match words[..at] {
        ["the", ref title @ ..] => title,
        ref title => title,
    };
    if title.is_empty() {
        return None;
    }
    let artist = match words.get(at + 1) {
        None => None,
        Some(&"by") if words.len() > at + 2 => Some(&words[at + 2..]),
        Some(_) => return None,
    };
    let phrase = match artist {
        Some(artist) => format!("{} by {}", title.join(" "), artist.join(" ")),
        None => title.join(" "),
    };
    build_request(&phrase)
}

/// Turns the text after "play" into a request, or `None` if there is none.
fn build_request(rest: &str) -> Option<PlayRequest> {
    let rest = without_trailing_aside(rest.trim());
    if rest.is_empty() {
        return None;
    }

    let splits = splits(rest);
    // The last " by " drives the query; the resolver still gets every split.
    let best = splits.first().cloned();
    let (title, artist) = match best {
        Some((title, artist)) => (Some(title.to_string()), Some(artist.to_string())),
        None => (None, None),
    };
    let query = match (&title, &artist) {
        (Some(title), Some(artist)) => format!("{title} by {artist}"),
        _ => rest.to_string(),
    };

    Some(PlayRequest {
        query,
        title,
        artist,
        splits,
    })
}

/// Every "<title> by <artist>" split, last " by " first ("Stand By Me" needs the others).
fn splits(phrase: &str) -> Vec<(String, String)> {
    let words: Vec<&str> = phrase.split_whitespace().collect();
    let mut out: Vec<(String, String)> = Vec::new();

    let at_by: Vec<usize> = words
        .iter()
        .enumerate()
        .filter(|(_, w)| **w == "by")
        .map(|(i, _)| i)
        .collect();
    for i in at_by.into_iter().rev() {
        let title = words[..i].join(" ");
        let artist = words[i + 1..].join(" ");
        if title.is_empty() || artist.is_empty() {
            continue;
        }
        out.push((title, artist));
    }
    out
}

/// Every reading of a phrase: its [`splits`] plus the whole phrase.
pub fn readings(phrase: &str) -> Vec<(Option<String>, Option<String>)> {
    let mut out: Vec<(Option<String>, Option<String>)> = Vec::new();
    for reading in splits(phrase)
        .into_iter()
        .map(|(title, artist)| (Some(title), Some(artist)))
        .chain([(None, Some(phrase.to_string()))])
    {
        if !out.contains(&reading) {
            out.push(reading);
        }
    }
    out
}

/// Lowercases and strips punctuation, collapsing whitespace.
fn normalize(text: &str) -> String {
    let filtered: String = text
        .chars()
        .map(|c| c.to_ascii_lowercase())
        .filter(|c| c.is_alphanumeric() || c.is_whitespace())
        .collect();
    filtered.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
#[path = "tests/parser.rs"]
mod tests;
