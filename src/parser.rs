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

/// The command word that must come straight after the wake word.
const PLAY_VERB: &str = "play";

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

/// Turns transcripts into [`PlayRequest`]s: "<wake word> play <request>", nothing else.
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

    /// Whether `words[at]` is a wake word with "play" straight after it.
    fn starts_command(&self, words: &[&str], at: usize) -> bool {
        self.is_wake_word(words[at]) && words.get(at + 1) == Some(&PLAY_VERB)
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

    /// Whether the wake model's window holds "<wake word> play".
    pub fn wakes_on(&self, window: &str) -> bool {
        let text = normalize(window);
        let words: Vec<&str> = text.split(' ').filter(|w| !w.is_empty()).collect();
        (0..words.len()).any(|at| self.starts_command(&words, at))
    }

    /// The words after the first "<wake word> play" are the request.
    pub fn command(&self, transcript: &str) -> Result<PlayRequest, ParseMiss> {
        let text = normalize(transcript);
        let words: Vec<&str> = text.split(' ').filter(|w| !w.is_empty()).collect();
        let Some(at) = (0..words.len()).find(|&at| self.starts_command(&words, at)) else {
            return Err(if words.iter().any(|w| self.is_wake_word(w)) {
                ParseMiss::NoPlayVerb
            } else {
                ParseMiss::NoWakeWord
            });
        };
        build_request(&words[at + 2..].join(" ")).ok_or(ParseMiss::EmptyRequest)
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
