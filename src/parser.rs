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
    /// Every plausible reading of the request as "<title> by <artist>".
    ///
    /// `title`/`artist` are the last-" by "-split guess, which is wrong whenever
    /// "by" belongs to the title. This is the full set, so the resolver can try
    /// all of them instead of committing to the guess. Empty when the phrase
    /// has no " by " in it.
    pub splits: Vec<(String, String)>,
}

/// The command word that must come straight after the wake word.
const PLAY_VERB: &str = "play";

/// Spoken asides Whisper appends after the request, which are not its title.
///
/// "play look at me, come on" arrived as "late look at me. come on"; the
/// trailing frustration made the catalog search and the matcher both worse.
/// A title that is only the aside ("play come on") is left alone.
const TRAILING_ASIDES: &[&str] = &["come on"];

/// Drops a trailing aside, but only when a request precedes it and it starts
/// on a word boundary, so "welcome on" does not become "wel".
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

    /// The request alone, with any wake word and "play" removed.
    ///
    /// A rewriting model asked to tidy a transcript tends to keep the wake word,
    /// and the search would then be aimed at "groovy dracula" rather than
    /// "Dracula". What comes back may be the whole command, the request on its
    /// own, or the wake word with no "play", so a leading wake word and "play"
    /// are dropped either way. `None` only when there is nothing to search for.
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

    /// Whether the wake model's window holds "<wake word> play", which is what
    /// the acknowledgement tone and the lowered music promise.
    pub fn wakes_on(&self, window: &str) -> bool {
        let text = normalize(window);
        let words: Vec<&str> = text.split(' ').filter(|w| !w.is_empty()).collect();
        (0..words.len()).any(|at| self.starts_command(&words, at))
    }

    /// The grammar applied to a whole utterance the wake gate woke on: the words
    /// after the first "<wake word> play" are the request.
    ///
    /// [`ParseMiss::EmptyRequest`] is "groovy play" with nothing after it, which
    /// the pipeline answers by waiting for the request.
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

    /// The request in an utterance that followed "groovy play" and a pause.
    ///
    /// The speaker has already been acknowledged, so a repeated wake word or
    /// "play" at the front is dropped rather than searched for.
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
    ///
    /// A natural pause inside a command splits one sentence across two
    /// utterances — "groovy play", pause, "lofi hip hop" — and the second half
    /// arrives with no wake word in it. This turns that trailing half into the
    /// request the first half was waiting for.
    pub fn request_from_query(&self, text: &str) -> Option<PlayRequest> {
        typed_request(text)
    }
}

/// Reads text that is already just the request, with no wake word or "play".
///
/// Cleaned exactly the way the request part of a spoken command is, so the same
/// words typed into `/play` and said out loud come out as the same
/// [`PlayRequest::query`]. The song memory keys on that query, which is what
/// lets a phrase taught by typing be found when spoken, and the other way round.
pub fn typed_request(text: &str) -> Option<PlayRequest> {
    build_request(&normalize(text))
}

/// Words that, said before the title, ask for a whole album: "play the album
/// 17". Longest first, so "the full album" is not read as "the" + "full album".
const ALBUM_LEADS: &[&[&str]] = &[
    &["the", "full", "album"],
    &["the", "whole", "album"],
    &["the", "album"],
    &["full", "album"],
    &["whole", "album"],
    &["album"],
];

/// The album a request asks for by saying "album", if it does.
///
/// The speaker said what they want, so nothing has to be guessed: the bot goes
/// straight to the albums. Three ways of saying it:
///
/// - before the title: "the album 17 by xxxtentacion", "the full album 17",
///   "album called 17";
/// - after the title: "the 17 album by xxxtentacion", "the slow rush album";
/// - at the very end: "17 by xxxtentacion album".
///
/// `None` for anything else, including "the album" with no title after it,
/// which is left to be searched for as words, and "the album version of …",
/// which is a song.
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
    // The last " by " is the best guess and drives the query, because it is
    // right whenever the speaker meant "<title> by <artist>" and a multi-word
    // artist cannot be split any other way. It is only a guess: "stand by me"
    // splits into title "stand" and artist "me", and the resolver is given
    // every plausible split so it does not have to commit to that one.
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

/// Every plausible reading of a phrase as "<title> by <artist>", best first.
///
/// "by" is part of far more song titles than it is a separator: *Stand By Me*,
/// *Take Me By The Hand*, *By The Way*, *Hey Jude By The Beatles*. Committing
/// to the last " by " turns "play stand by me" into a search for "stand" by an
/// artist called "me", which matches nothing and then plays whatever scores
/// best. So every position of " by " is offered, plus the reading where the
/// whole phrase is the title and no artist was named at all.
///
/// Ordered so the last " by " comes first, because that is the reading the
/// query and the backwards-compatible `title`/`artist` fields use.
///
/// Returns an empty vec when the phrase has no " by " in it: the whole phrase
/// is then the only reading, and there is nothing to choose between.
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

/// The readings of a phrase, including the whole phrase as its own title.
///
/// [`splits`] only answers "<title> by <artist>"; most requests name no artist
/// at all, and the whole phrase is then the title. Returned as a single
/// candidate rather than an `Option` so the resolver can treat every reading
/// the same way.
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
