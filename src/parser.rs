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

/// Wake words this short must be heard exactly, so short words like "bot"
/// cannot match everyday speech ("not", "boy", "hot").
///
/// "bot" used to answer to one swapped vowel as well, because the small model
/// often writes it as "but". That made "but", "bet", "bit" and "bat" wake the
/// bot, and "but" opens half the sentences in a conversation.
const EXACT_MATCH_MAX_LEN: usize = 4;

/// The command word that turns a phrase into a play request.
const PLAY_VERB: &str = "play";

/// Ways Whisper has actually written "play" in a live channel.
///
/// One edit covers "lay", "pley" and "paly", but not the two- and three-edit
/// forms below. They are listed because a real request was refused on each:
/// the wake gate heard "late" and the parser had nothing to treat as the verb.
const PLAY_ALIASES: &[&str] = &["late", "plate"];

/// Spoken asides Whisper appends after the request, which are not its title.
///
/// "play look at me, come on" arrived as "late look at me. come on"; the
/// trailing frustration made the catalog search and the matcher both worse.
/// A title that is only the aside ("play come on") is left alone.
const TRAILING_ASIDES: &[&str] = &["come on"];

/// Single-character mistakes tolerated in longer wake words.
///
/// Speech-to-text reliably mangles a proper name — "shamash" can come back as
/// "shemash", "samash" or "shammash" — and rejecting those makes the bot feel
/// broken, so one edit is allowed on words long enough. Such a match is only a
/// [`WakeMatch::Near`] one, though: one letter from "groovy" is also "groove".
const FUZZY_MAX_EDITS: usize = 1;

/// How closely a heard word matched a wake word.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WakeMatch {
    /// Heard exactly as configured.
    Exact,
    /// One letter off a long wake word. Speech-to-text writes a name it does
    /// not know this way ("shemash"), but an ordinary word one letter from a
    /// wake word ("groove" for "groovy") looks just the same. So a near match
    /// only counts with "play" straight after it.
    Near,
}

/// Levenshtein distance between two words, used for wake-word matching.
fn edit_distance(left: &str, right: &str) -> usize {
    let right: Vec<char> = right.chars().collect();
    let mut previous: Vec<usize> = (0..=right.len()).collect();
    let mut current = vec![0; right.len() + 1];

    for (i, l) in left.chars().enumerate() {
        current[0] = i + 1;
        for (j, r) in right.iter().enumerate() {
            let cost = usize::from(l != *r);
            current[j + 1] = (previous[j] + cost)
                .min(previous[j + 1] + 1)
                .min(current[j] + 1);
        }
        std::mem::swap(&mut previous, &mut current);
    }
    previous[right.len()]
}

/// Whether `word` is the play verb, allowing the damage speech-to-text does to
/// a four-letter word: "play" comes back as "lay", "pley" or "paly".
fn is_play_verb(word: &str) -> bool {
    word == PLAY_VERB
        || PLAY_ALIASES.contains(&word)
        || (word.chars().count() >= 3 && edit_distance(word, PLAY_VERB) <= 1)
}

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

/// Turns transcripts into [`PlayRequest`]s using a wake word + "play" grammar.
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

    /// Whether `word` is one of the configured wake words.
    ///
    /// Long wake words tolerate a single character of speech-to-text damage;
    /// short ones must match exactly, to avoid firing on ordinary speech.
    pub fn is_wake_word(&self, word: &str) -> bool {
        self.wake_match(word).is_some()
    }

    /// How closely `word` matched the closest wake word, if at all.
    fn wake_match(&self, word: &str) -> Option<WakeMatch> {
        if self.wake_words.contains(word) {
            return Some(WakeMatch::Exact);
        }
        self.wake_words
            .iter()
            .any(|wake| {
                wake.chars().count() > EXACT_MATCH_MAX_LEN
                    && edit_distance(word, wake) <= FUZZY_MAX_EDITS
            })
            .then_some(WakeMatch::Near)
    }

    /// Every wake word in `words`, in order, with how closely it matched.
    fn wake_positions<'a>(
        &'a self,
        words: &'a [&str],
    ) -> impl Iterator<Item = (usize, WakeMatch)> + 'a {
        words
            .iter()
            .enumerate()
            .filter_map(|(at, word)| self.wake_match(word).map(|strength| (at, strength)))
    }

    /// The words of the request, if the wake word at `at` starts a command.
    ///
    /// Two shapes are a command:
    ///
    /// - The wake word with "play" straight after it, anywhere in the
    ///   sentence: "okay so groovy, play Dracula".
    /// - An exact wake word that opens the sentence, after nothing but a
    ///   greeting. "play" is then optional and may come later: "groovy,
    ///   Dracula", "hey groovy, could you play Dracula".
    ///
    /// Anything else is someone talking: "I told groovy to play something",
    /// "the groove in this song". The returned words may be empty, for a wake
    /// word said on its own.
    fn tail_at<'a, 'w>(
        &self,
        words: &'w [&'a str],
        at: usize,
        strength: WakeMatch,
    ) -> Option<&'w [&'a str]> {
        let after = &words[at + 1..];
        if words[at] == PLAY_VERB {
            return Some(after);
        }
        if let Some(rest) = after_direct_play(after) {
            return Some(rest);
        }
        let opens = words[..at].iter().all(|w| GREETINGS.contains(w));
        if strength == WakeMatch::Exact && opens {
            return Some(match after.iter().position(|w| *w == PLAY_VERB) {
                Some(play) => &after[play + 1..],
                None => strip_fillers(after),
            });
        }
        None
    }

    /// Requires a wake word followed by "play <something>".
    ///
    /// "play" doubles as its own wake word when it is configured as one, so
    /// "play <query>" works on its own while "groovy play <query>" still does.
    /// Recognizes "play <title> by <artist>" (the artist gets the search bias)
    /// and falls back to the raw text after "play" as the query.
    pub fn parse(&self, transcript: &str) -> Option<PlayRequest> {
        self.parse_with_reason(transcript).ok()
    }

    /// Like [`Self::parse`], but reports why the transcript was rejected.
    pub fn parse_with_reason(&self, transcript: &str) -> Result<PlayRequest, ParseMiss> {
        let text = normalize(transcript);
        let tail = self.command_tail(&text)?;
        build_request(&tail).ok_or(ParseMiss::EmptyRequest)
    }

    /// The words after the wake word and "play", if the grammar matched.
    ///
    /// Split out because two callers need the same index arithmetic: the parser
    /// itself, and [`Self::strip_command`], which the transcript-rewriting stage
    /// uses when a model returns a whole sentence instead of just the request.
    fn command_tail(&self, text: &str) -> Result<String, ParseMiss> {
        let words: Vec<&str> = text.split(' ').collect();

        let wake_idx = words
            .iter()
            .position(|w| self.is_wake_word(w))
            .ok_or(ParseMiss::NoWakeWord)?;
        // An exact "play" after the wake word wins, so "lay play despacito"
        // keeps "play" out of the query. Only when there is none does a
        // misheard verb count, including the wake word itself.
        let play_at = words[wake_idx + 1..]
            .iter()
            .position(|w| *w == PLAY_VERB)
            .map(|offset| wake_idx + 1 + offset)
            .or_else(|| is_play_verb(words[wake_idx]).then_some(wake_idx))
            .or_else(|| {
                words[wake_idx + 1..]
                    .iter()
                    .position(|w| is_play_verb(w))
                    .map(|offset| wake_idx + 1 + offset)
            })
            .ok_or(ParseMiss::NoPlayVerb)?;

        // Only an exact "play" with nothing after it means "I am about to say
        // the title": the pipeline arms itself to take the next utterance. A
        // lone "late" is far more likely "I'm late" than a deliberate pause.
        if play_at + 1 == words.len() && words[play_at] != PLAY_VERB {
            return Err(ParseMiss::NoPlayVerb);
        }

        Ok(words[play_at + 1..].join(" "))
    }

    /// The request alone, with any wake word and "play" removed.
    ///
    /// A rewriting model asked to tidy a transcript tends to keep the wake word,
    /// and the search would then be aimed at "groovy dracula" rather than
    /// "Dracula". Two passes, because what comes back from a model may be any of
    /// three shapes: the command as it was spoken with the wake word somewhere in
    /// it, the request on its own, or the wake word with no "play". The grammar
    /// handles the first and the tolerant pass the other two, so all three come
    /// back as the same request. `None` only when there is nothing to search for.
    pub fn strip_command(&self, transcript: &str) -> Option<String> {
        let text = normalize(transcript);

        if let Ok(Some(request)) = self.command_tail(&text).map(|tail| build_request(&tail)) {
            return Some(request.query);
        }

        let mut words: Vec<&str> = text.split(' ').collect();
        if words.len() == 1 && words[0].is_empty() {
            return None;
        }
        if self.is_wake_word(words[0]) {
            words.remove(0);
        }
        if words.first().is_some_and(|w| is_play_verb(w)) {
            words.remove(0);
        }
        build_request(&words.join(" ")).map(|request| request.query)
    }

    /// Whether the wake model's window could be the start of a command, which
    /// is what the acknowledgement tone and the lowered music promise.
    ///
    /// The rules of [`Self::command`], applied to the two seconds the wake
    /// model sees. The window may have cut off the start of the sentence, so
    /// this can wake on what the grammar then turns down ("that's groovy",
    /// when the window starts at "groovy"), but whatever the grammar accepts,
    /// it wakes on. The common miss it does turn away is a wake word in the
    /// middle of what someone is saying, with no "play" after it.
    pub fn wakes_on(&self, window: &str) -> bool {
        let text = normalize(window);
        let words: Vec<&str> = text.split(' ').filter(|w| !w.is_empty()).collect();
        self.wake_positions(&words)
            .any(|(at, strength)| self.tail_at(&words, at, strength).is_some())
    }

    /// The grammar applied to a whole utterance the wake gate woke on.
    ///
    /// Looser than [`Self::parse_with_reason`] in one way: "play" is optional
    /// when an exact wake word opens the utterance, so "groovy, Dracula by Tame
    /// Impala" is a request. Stricter in another: anywhere else, "play" has to
    /// come straight after the wake word, which keeps "I told groovy to play
    /// something" from being searched for. See [`Self::tail_at`].
    ///
    /// Every wake word in the utterance is tried in turn, so "that groove was
    /// nice, groovy play Dracula" still finds the request.
    /// [`ParseMiss::EmptyRequest`] is the wake word said on its own, which the
    /// pipeline answers by waiting for the request.
    pub fn command(&self, transcript: &str) -> Result<PlayRequest, ParseMiss> {
        let text = normalize(transcript);
        let words: Vec<&str> = text.split(' ').filter(|w| !w.is_empty()).collect();
        let mut miss = ParseMiss::NoWakeWord;
        for (at, strength) in self.wake_positions(&words) {
            let Some(tail) = self.tail_at(&words, at, strength) else {
                if miss == ParseMiss::NoWakeWord {
                    miss = ParseMiss::NoPlayVerb;
                }
                continue;
            };
            match build_request(&tail.join(" ")) {
                Some(request) => return Ok(request),
                // A bare wake word arms the speaker, so it wins over one that
                // was only part of a sentence.
                None => miss = ParseMiss::EmptyRequest,
            }
        }
        Err(miss)
    }

    /// The request in an utterance that followed a bare wake word.
    ///
    /// The speaker has already been acknowledged, so a repeated wake word or
    /// "play" at the front is dropped rather than searched for.
    pub fn armed_request(&self, transcript: &str) -> Option<PlayRequest> {
        let text = normalize(transcript);
        let mut words: Vec<&str> = text.split(' ').filter(|w| !w.is_empty()).collect();
        while let Some(first) = words.first() {
            if self.is_wake_word(first) || GREETINGS.contains(first) || *first == PLAY_VERB {
                words.remove(0);
            } else {
                break;
            }
        }
        build_request(&strip_fillers(&words).join(" "))
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

/// Words that may come before a wake word without making it mid-sentence.
const GREETINGS: &[&str] = &["hey", "hi", "ok", "okay", "yo", "oi", "hello"];

/// Words between the wake word and a request that are not part of it. Kept
/// short on purpose: "you" or "now" would also strip the start of real titles
/// ("You Belong With Me"), and "could you play …" already goes through "play".
const FILLERS: &[&str] = &["please", "play"];

fn strip_fillers<'a, 'b>(words: &'b [&'a str]) -> &'b [&'a str] {
    let skip = words.iter().take_while(|w| FILLERS.contains(w)).count();
    &words[skip..]
}

/// The words after a "play" that comes straight after the wake word, with a
/// "please" allowed in between: "groovy play …", "groovy please play …".
fn after_direct_play<'a, 'b>(after: &'b [&'a str]) -> Option<&'b [&'a str]> {
    let skip = after.iter().take_while(|w| **w == "please").count();
    (after.get(skip).copied() == Some(PLAY_VERB)).then(|| &after[skip + 1..])
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
