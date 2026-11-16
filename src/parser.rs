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
mod tests {
    use super::*;

    fn parser() -> CommandParser {
        CommandParser::new(["shamash", "groovy"].into_iter().map(str::to_string))
    }

    /// A parser with "play" acting as a wake word of its own.
    fn parser_with_play_wake_word() -> CommandParser {
        CommandParser::new(["groovy", "play"].into_iter().map(str::to_string))
    }

    /// A parser configured with "bot" and its clipped "ut" and "ot" forms, as
    /// someone whose voice needs them might set `WAKE_WORDS`.
    fn parser_with_short_wake_words() -> CommandParser {
        CommandParser::new(["bot", "play", "ut", "ot"].into_iter().map(str::to_string))
    }

    #[test]
    fn hears_a_wake_word_that_lost_its_first_letter() {
        for heard in [
            "ut play despacito",
            "ot play despacito",
            "bot play despacito",
        ] {
            let req = parser_with_short_wake_words()
                .parse(heard)
                .unwrap_or_else(|| panic!("{heard:?} should have been heard"));
            assert_eq!(req.query, "despacito", "for {heard:?}");
        }
    }

    #[test]
    fn a_clipped_wake_word_is_not_invented_when_not_configured() {
        let bot_only = CommandParser::new(["bot".to_string()]);
        assert_eq!(
            bot_only.parse_with_reason("ut play despacito"),
            Err(ParseMiss::NoWakeWord)
        );
    }

    #[test]
    fn play_alone_is_a_whole_command_when_it_is_a_wake_word() {
        let req = parser_with_play_wake_word()
            .parse("play dracula by tame impala")
            .unwrap();
        assert_eq!(req.query, "dracula by tame impala");
        assert_eq!(req.title.as_deref(), Some("dracula"));
        assert_eq!(req.artist.as_deref(), Some("tame impala"));
    }

    #[test]
    fn play_keeps_working_after_another_wake_word() {
        let req = parser_with_play_wake_word()
            .parse("groovy play despacito")
            .unwrap();
        assert_eq!(req.query, "despacito");
    }

    #[test]
    fn a_lone_play_wake_word_still_needs_a_query() {
        assert_eq!(
            parser_with_play_wake_word().parse_with_reason("play"),
            Err(ParseMiss::EmptyRequest)
        );
    }

    #[test]
    fn extracts_title_and_artist() {
        let req = parser()
            .parse("Shamash, play Dracula by Tame Impala")
            .unwrap();
        assert_eq!(req.query, "dracula by tame impala");
        assert_eq!(req.title.as_deref(), Some("dracula"));
        assert_eq!(req.artist.as_deref(), Some("tame impala"));
    }

    #[test]
    fn tolerates_greeting_words_after_wake_word() {
        let req = parser()
            .parse("Hey shamash, could you please play Bohemian Rhapsody!")
            .unwrap();
        assert_eq!(req.query, "bohemian rhapsody");
        assert_eq!(req.title, None);
    }

    /// The live failure this pair of fixes came from: the wake gate fired on
    /// "late", Whisper ended the request with "come on", and nothing played.
    #[test]
    fn a_late_for_play_and_a_trailing_come_on_still_parse() {
        let parser = CommandParser::new(["groovy", "play", "plate", "lay"].map(str::to_string));
        let req = parser.parse("late look at me. come on").unwrap();
        assert_eq!(req.query, "look at me");
    }

    /// A lone misheard verb must not arm the "next utterance is the title"
    /// path; only an exact "play" pause does that.
    #[test]
    fn a_lone_misheard_play_does_not_arm_the_next_utterance() {
        let parser = CommandParser::new(["groovy", "play", "plate", "lay"].map(str::to_string));
        assert_eq!(parser.parse_with_reason("late"), Err(ParseMiss::NoPlayVerb));
        assert_eq!(
            parser.parse_with_reason("i'm late"),
            Err(ParseMiss::NoPlayVerb)
        );
        assert_eq!(
            parser.parse_with_reason("play"),
            Err(ParseMiss::EmptyRequest)
        );
    }

    /// A title that is only the aside, or merely ends in those letters, is
    /// never eaten: "come on" and "welcome on" are both plausible queries.
    #[test]
    fn a_title_that_looks_like_the_aside_is_kept() {
        assert_eq!(
            parser().parse("groovy play come on").unwrap().query,
            "come on"
        );
        assert_eq!(
            parser().parse("groovy play welcome on").unwrap().query,
            "welcome on"
        );
    }

    #[test]
    fn requires_a_wake_word() {
        assert!(parser().parse("play dracula by tame impala").is_none());
    }

    #[test]
    fn requires_the_word_play() {
        assert!(parser().parse("shamash what's the weather").is_none());
    }

    #[test]
    fn falls_back_to_raw_query_without_artist() {
        let req = parser().parse("groovy play despacito").unwrap();
        assert_eq!(req.query, "despacito");
        assert_eq!(req.title, None);
        assert_eq!(req.artist, None);
    }

    #[test]
    fn empty_request_after_play_is_rejected() {
        assert!(parser().parse("groovy play").is_none());
    }

    #[test]
    fn splits_on_last_by_for_multi_word_artist() {
        let req = parser()
            .parse("shamash play us and them by pink floyd")
            .unwrap();
        assert_eq!(req.title.as_deref(), Some("us and them"));
        assert_eq!(req.artist.as_deref(), Some("pink floyd"));
    }

    /// "by" belongs to more song titles than it separates artists, so the last
    /// " by " is only ever a guess. Every position is kept, best first, so the
    /// resolver can try them all.
    #[test]
    fn every_position_of_by_is_kept_as_a_reading() {
        for (heard, expected) in [
            ("stand by me", vec![("stand".to_string(), "me".to_string())]),
            (
                "take me by the hand",
                vec![("take me".to_string(), "the hand".to_string())],
            ),
            (
                "stand by me by ben e king",
                vec![
                    ("stand by me".to_string(), "ben e king".to_string()),
                    ("stand".to_string(), "me by ben e king".to_string()),
                ],
            ),
        ] {
            let req = parser()
                .parse(&format!("shamash play {heard}"))
                .unwrap_or_else(|| panic!("{heard} should parse"));
            assert_eq!(req.splits, expected, "for {heard:?}");
        }
    }

    /// The order is what the backwards-compatible `title`/`artist` fields use,
    /// and it has to stay last-" by "-first: "us and them by pink floyd" can
    /// only mean a two-word artist.
    #[test]
    fn the_last_by_stays_the_primary_guess() {
        let req = parser()
            .parse("shamash play stand by me by ben e king")
            .unwrap();
        assert_eq!(req.title.as_deref(), Some("stand by me"));
        assert_eq!(req.artist.as_deref(), Some("ben e king"));
        assert_eq!(req.query, "stand by me by ben e king");
    }

    /// A phrase with no " by " at all has exactly one reading, so the guess is
    /// no guess: nothing is split, and the whole phrase is the title.
    #[test]
    fn a_request_with_no_by_has_no_splits() {
        let req = parser().parse("shamash play despacito").unwrap();
        assert!(req.splits.is_empty());
        assert_eq!(req.title, None);
        assert_eq!(req.artist, None);
        assert_eq!(req.query, "despacito");

        let readings = readings("despacito");
        assert_eq!(readings, vec![(None, Some("despacito".to_string()))]);
    }

    /// "by" at either end is not a separator at all: there is nothing on one
    /// side of it.
    #[test]
    fn a_by_with_nothing_on_one_side_is_not_a_split() {
        for heard in ["by the way by daft punk", "hey jude by"] {
            let req = parser()
                .parse(&format!("shamash play {heard}"))
                .unwrap_or_else(|| panic!("{heard} should parse"));
            assert!(
                req.splits
                    .iter()
                    .all(|(t, a)| !t.is_empty() && !a.is_empty()),
                "{heard}: {:?}",
                req.splits
            );
        }
        // "by the way by daft punk" has a leading "by" that produces no split,
        // so only the one reading survives.
        let req = parser()
            .parse("shamash play by the way by daft punk")
            .unwrap();
        assert_eq!(
            req.splits,
            vec![("by the way".to_string(), "daft punk".to_string())]
        );
        // A trailing "by" produces no split either.
        let req = parser().parse("shamash play hey jude by").unwrap();
        assert!(req.splits.is_empty());
    }

    /// "By the Way" is a title that begins with "by" and nothing else, so the
    /// reading where the whole phrase is the title has to be available even
    /// when a " by " split exists.
    #[test]
    fn the_whole_phrase_is_always_one_of_the_readings() {
        let readings = readings("by the way by daft punk");
        assert_eq!(
            readings.last(),
            Some(&(None, Some("by the way by daft punk".into())))
        );
        assert!(
            readings.contains(&(Some("by the way".into()), Some("daft punk".into()))),
            "{readings:?}"
        );
    }

    /// The readings are deduplicated in order, so a phrase like "stand by me"
    /// -- where splitting at either "by" gives the same pair -- does not send
    /// the resolver the same query twice.
    #[test]
    fn duplicate_readings_are_collapsed() {
        let readings = readings("stand by me");
        assert_eq!(readings.len(), 2, "{readings:?}");
        assert_eq!(
            readings[0],
            (Some("stand".into()), Some("me".into())),
            "{readings:?}"
        );
        assert_eq!(readings[1], (None, Some("stand by me".into())));
    }

    #[test]
    fn case_and_punctuation_are_normalised() {
        let req = parser()
            .parse("GROOVY: PLAY \u{2019}talking\u{2019} BY Sade!")
            .unwrap();
        assert_eq!(req.query, "talking by sade");
    }

    #[test]
    fn play_after_wake_word_is_required() {
        assert!(parser().parse("play danger zone shamash").is_none());
    }

    #[test]
    fn empty_transcript_is_rejected() {
        assert!(parser().parse("   ").is_none());
    }

    #[test]
    fn reports_why_a_transcript_was_rejected() {
        let parser = parser();
        assert_eq!(
            parser.parse_with_reason("what is the weather"),
            Err(ParseMiss::NoWakeWord)
        );
        assert_eq!(
            parser.parse_with_reason("shamash what is the weather"),
            Err(ParseMiss::NoPlayVerb)
        );
        assert_eq!(
            parser.parse_with_reason("shamash play"),
            Err(ParseMiss::EmptyRequest)
        );
        assert!(parser.parse_with_reason("shamash play despacito").is_ok());
    }

    #[test]
    fn parse_agrees_with_parse_with_reason() {
        let parser = parser();
        for transcript in [
            "shamash play dracula by tame impala",
            "groovy play despacito",
            "no wake word here",
            "shamash play",
            "  ",
        ] {
            assert_eq!(
                parser.parse(transcript),
                parser.parse_with_reason(transcript).ok()
            );
        }
    }

    #[test]
    fn exposes_configured_wake_words() {
        let parser = parser();
        let words = parser.wake_words();
        assert!(words.contains("shamash"));
        assert!(words.contains("groovy"));
    }

    #[test]
    fn tolerates_one_letter_wake_word_mistakes() {
        let parser = parser();
        for heard in [
            "shemash play despacito",
            "samash play despacito",
            "shammash play despacito",
            "shamish play despacito",
            "Shamash, play despacito",
        ] {
            assert!(parser.parse(heard).is_some(), "should accept {heard:?}");
        }
    }

    /// "play" is a shipped default wake word, and a bare "play" stands in for
    /// the whole command so the shortest useful thing to say is "play lofi".
    ///
    /// The cost is that ordinary speech about playing something is taken as a
    /// request. This is a deliberate trade rather than a bug: the shorter
    /// command is worth more than avoiding a few unwanted searches, and the
    /// queue makes a false trigger cheap to skip past.
    ///
    /// These assertions exist so the behaviour is not "fixed" by someone who
    /// reads it as an oversight. A rejection list would only cover the phrases
    /// somebody thought of.
    #[test]
    fn a_bare_play_wake_word_takes_the_next_words_as_a_query() {
        let parser = parser_with_play_wake_word();
        for (heard, query) in [
            ("play despacito", "despacito"),
            ("play the game", "the game"),
            ("play guitar", "guitar"),
            ("play football tomorrow", "football tomorrow"),
            ("play again", "again"),
        ] {
            let request = parser.parse(heard).unwrap_or_else(|| {
                panic!(
                    "{heard:?} should parse, got {:?}",
                    parser.parse_with_reason(heard)
                )
            });
            assert_eq!(request.query, query, "wrong query for {heard:?}");
        }
    }

    /// The trade above does not extend to "play" with nothing after it, which
    /// is the case where there is no query to search for at all.
    #[test]
    fn a_bare_play_with_nothing_after_it_still_needs_a_title() {
        assert_eq!(
            parser_with_play_wake_word().parse_with_reason("play"),
            Err(ParseMiss::EmptyRequest)
        );
    }

    /// A wake word that is also a misheard "play" is both, so the common
    /// "lay despacito" and "late look at me" transcripts work.
    #[test]
    fn a_misheard_play_wake_word_is_also_the_verb() {
        let parser = CommandParser::new(["lay", "plate"].into_iter().map(str::to_string));
        assert_eq!(
            parser.parse_with_reason("lay despacito").unwrap().query,
            "despacito"
        );
        assert_eq!(
            parser.parse_with_reason("plate despacito").unwrap().query,
            "despacito"
        );
        // "a silver plate" does contain the wake word, so it gets as far as
        // wanting a verb; it has none, so it is rejected.
        assert!(parser.parse("a silver plate").is_none());
        assert!(parser.parse("i was laying the table").is_none());
        assert_eq!(
            parser.parse("lay play despacito").unwrap().query,
            "despacito"
        );
        assert_eq!(
            parser.parse("plate play despacito").unwrap().query,
            "despacito"
        );
    }

    /// Wake words are lowercased when configured, so a capitalised entry is not
    /// a second word and does not change what is heard.
    #[test]
    fn a_capitalised_wake_word_is_the_same_word() {
        let parser = CommandParser::new(["Plate", "LAY"].into_iter().map(str::to_string));
        assert!(parser.parse("lay play despacito").is_some());
        assert!(parser.parse("plate play despacito").is_some());
    }

    /// A short wake word must be heard exactly. "bot" used to answer to one
    /// swapped vowel too, which made every "but" in a conversation wake it.
    #[test]
    fn a_short_wake_word_must_be_heard_exactly() {
        let parser = CommandParser::new(["bot".to_string()]);
        for heard in [
            "not play despacito",
            "boy play despacito",
            "hot play despacito",
            "top play despacito",
            "but play despacito",
            "bat play despacito",
            "bet play despacito",
            "bit play despacito",
            "i will not play despacito today",
        ] {
            assert_eq!(
                parser.parse_with_reason(heard).err(),
                Some(ParseMiss::NoWakeWord),
                "should reject {heard:?}"
            );
        }
        assert_eq!(
            parser.parse("Bot play despacito").unwrap().query,
            "despacito"
        );
    }

    #[test]
    fn builds_a_request_from_the_half_that_followed_a_pause() {
        let parser = parser();
        let req = parser.request_from_query("Lo-fi hip hop.").unwrap();
        assert_eq!(req.query, "lofi hip hop");

        let req = parser.request_from_query("Dracula by Tame Impala").unwrap();
        assert_eq!(req.query, "dracula by tame impala");
        assert_eq!(req.title.as_deref(), Some("dracula"));
        assert_eq!(req.artist.as_deref(), Some("tame impala"));
    }

    #[test]
    fn a_pause_with_nothing_after_it_is_not_a_request() {
        let parser = parser();
        assert!(parser.request_from_query("   ").is_none());
        assert!(parser.request_from_query("").is_none());
    }

    #[test]
    fn ignores_wake_words_that_are_too_far_off() {
        let parser = parser();
        assert_eq!(
            parser.parse_with_reason("chameleon play despacito").err(),
            Some(ParseMiss::NoWakeWord)
        );
    }

    #[test]
    fn a_paused_request_without_a_wake_word_is_still_a_request() {
        assert_eq!(
            parser().strip_command("dracula by tame impala").as_deref(),
            Some("dracula by tame impala")
        );
    }

    /// The rewriting stage is handed whole sentences, wake word included, so the
    /// grammar has to be reusable on what comes back from a model.
    #[test]
    fn stripping_the_command_leaves_the_request() {
        let parser = parser();
        assert_eq!(
            parser
                .strip_command("Shamash, play Dracula by Tame Impala!")
                .as_deref(),
            Some("dracula by tame impala")
        );
        assert_eq!(
            parser.strip_command("play lofi hip hop").as_deref(),
            Some("lofi hip hop")
        );
        // A model may drop "play", or keep the wake word without it.
        assert_eq!(
            parser
                .strip_command("groovy dracula by tame impala")
                .as_deref(),
            Some("dracula by tame impala")
        );
        assert_eq!(
            parser.strip_command("shamash play").as_deref(),
            None,
            "nothing left to search for"
        );
        assert_eq!(parser.strip_command("   "), None);
    }

    /// Whatever the grammar accepts, stripping and parsing must agree on.
    #[test]
    fn stripping_agrees_with_parsing() {
        let parser = parser_with_play_wake_word();
        for transcript in [
            "groovy play dracula by tame impala",
            "play despacito",
            "shamash play us and them by pink floyd",
            "play",
        ] {
            assert_eq!(
                parser.strip_command(transcript),
                parser.parse(transcript).map(|r| r.query),
                "for {transcript:?}"
            );
        }
    }

    #[test]
    fn computes_edit_distance() {
        assert_eq!(edit_distance("shamash", "shamash"), 0);
        assert_eq!(edit_distance("shamash", "shemash"), 1);
        assert_eq!(edit_distance("shamash", "shammash"), 1);
        assert_eq!(edit_distance("shamash", "shamish"), 1);
        assert_eq!(edit_distance("flaw", "lawn"), 2);
        assert_eq!(edit_distance("kitten", "sitting"), 3);
        assert_eq!(edit_distance("", "bot"), 3);
    }

    /// The request the old grammar threw away: "groovy", then the title with no
    /// "play" in between.
    #[test]
    fn command_does_not_need_play_when_the_wake_word_opens_the_sentence() {
        let req = parser().command("Groovy, Dracula by Tame Impala").unwrap();
        assert_eq!(req.title.as_deref(), Some("dracula"));
        assert_eq!(req.artist.as_deref(), Some("tame impala"));
        let req = parser()
            .command("hey shamash please bohemian rhapsody")
            .unwrap();
        assert_eq!(req.query, "bohemian rhapsody");
    }

    #[test]
    fn command_still_understands_play() {
        assert_eq!(
            parser().command("groovy play dracula").unwrap().query,
            "dracula"
        );
        assert_eq!(
            parser()
                .command("shamash could you play bohemian rhapsody")
                .unwrap()
                .query,
            "bohemian rhapsody"
        );
        assert_eq!(
            parser_with_play_wake_word()
                .command("play stand by me")
                .unwrap()
                .query,
            "stand by me"
        );
    }

    #[test]
    fn command_needs_play_after_a_wake_word_mid_sentence() {
        assert_eq!(
            parser().command("i told groovy yesterday about it"),
            Err(ParseMiss::NoPlayVerb)
        );
        assert_eq!(
            parser()
                .command("okay so groovy play dracula")
                .unwrap()
                .query,
            "dracula"
        );
        assert_eq!(
            parser()
                .command("right groovy please play dracula")
                .unwrap()
                .query,
            "dracula"
        );
    }

    /// "play" somewhere later in the sentence is not enough mid-sentence: that
    /// is someone describing a request, not making one.
    #[test]
    fn a_later_play_mid_sentence_is_not_a_request() {
        for heard in [
            "i told groovy to play something",
            "you have to ask groovy and then play it",
            "that is so groovy let me play it again",
        ] {
            assert_eq!(
                parser().command(heard),
                Err(ParseMiss::NoPlayVerb),
                "for {heard:?}"
            );
        }
    }

    /// One letter off is how a name gets misheard, and also how an ordinary
    /// word looks. So a near match needs "play" straight after it, and never
    /// arms the speaker on its own.
    #[test]
    fn a_near_wake_word_needs_play_straight_after_it() {
        assert_eq!(
            parser().command("shemash play dracula").unwrap().query,
            "dracula"
        );
        assert_eq!(
            parser().command("groove play dracula").unwrap().query,
            "dracula"
        );
        for heard in [
            "groove is in the heart",
            "shemash dracula by tame impala",
            "groove could you play dracula",
            "groove",
        ] {
            assert_eq!(
                parser().command(heard),
                Err(ParseMiss::NoPlayVerb),
                "for {heard:?}"
            );
        }
    }

    /// The first wake word heard is not the only one tried.
    #[test]
    fn a_later_wake_word_is_still_heard() {
        assert_eq!(
            parser()
                .command("that groove was nice groovy play dracula")
                .unwrap()
                .query,
            "dracula"
        );
        // A wake word and "play" with the title still to come arms the
        // speaker, even after a wake word that was only conversation.
        assert_eq!(
            parser().command("the groove so groovy play"),
            Err(ParseMiss::EmptyRequest)
        );
    }

    #[test]
    fn a_bare_wake_word_is_an_empty_request() {
        assert_eq!(parser().command("Groovy."), Err(ParseMiss::EmptyRequest));
        assert_eq!(
            parser().command("groovy play"),
            Err(ParseMiss::EmptyRequest)
        );
        assert_eq!(parser().command("lunch"), Err(ParseMiss::NoWakeWord));
    }

    #[test]
    fn an_armed_request_drops_a_repeated_wake_word_and_play() {
        let p = parser();
        assert_eq!(
            p.armed_request("groovy play dracula by tame impala")
                .unwrap()
                .query,
            "dracula by tame impala"
        );
        assert_eq!(
            p.armed_request("You Belong With Me").unwrap().query,
            "you belong with me"
        );
        assert!(p.armed_request("groovy").is_none());
    }

    fn album_of(spoken: &str) -> Option<String> {
        let request = parser().command(spoken).ok()?;
        album_request(&request).map(|album| album.query)
    }

    /// Saying "album" asks for the album, wherever it is said.
    #[test]
    fn saying_album_names_the_album() {
        for (spoken, album) in [
            (
                "groovy play the album 17 by xxxtentacion",
                "17 by xxxtentacion",
            ),
            ("groovy play album 17 by xxxtentacion", "17 by xxxtentacion"),
            ("groovy play the full album 17", "17"),
            (
                "groovy play the whole album currents by tame impala",
                "currents by tame impala",
            ),
            ("groovy play the album called blonde", "blonde"),
            (
                "groovy play the 17 album by xxxtentacion",
                "17 by xxxtentacion",
            ),
            ("groovy play the slow rush album", "slow rush"),
            ("groovy play 17 by xxxtentacion album", "17 by xxxtentacion"),
            (
                "Groovy, the album Thriller by Michael Jackson.",
                "thriller by michael jackson",
            ),
        ] {
            assert_eq!(album_of(spoken).as_deref(), Some(album), "for {spoken:?}");
        }
    }

    /// Without "album", it is a request like any other, and whether it is an
    /// album is for the catalog to say.
    #[test]
    fn a_request_without_album_is_not_named_as_one() {
        for spoken in [
            "groovy play 17 by xxxtentacion",
            "groovy play the album",
            "groovy play album",
            "groovy play an album nobody likes",
            "groovy play the album version of dracula",
        ] {
            assert_eq!(album_of(spoken), None, "for {spoken:?}");
        }
    }

    /// Typed words come out as the same query the same words spoken do.
    #[test]
    fn a_typed_request_reads_like_the_same_words_spoken() {
        let p = parser();
        for (typed, spoken) in [
            (
                "Dracula by Tame Impala",
                "Shamash, play dracula by tame impala.",
            ),
            ("Don't Stop Me Now!", "groovy play don't stop me now"),
            ("Stand By Me", "groovy play stand by me"),
        ] {
            assert_eq!(
                typed_request(typed).map(|r| r.query),
                p.parse(spoken).map(|r| r.query),
                "for {typed:?}"
            );
        }
        assert!(typed_request("?!").is_none());
    }

    #[test]
    fn the_gate_ignores_punctuation_and_case() {
        assert!(parser().wakes_on("Hey, GROOVY!"));
        assert!(!parser().wakes_on("not now"));
    }

    /// The gate wakes on whatever could be the start of a command.
    #[test]
    fn the_gate_wakes_on_what_could_start_a_command() {
        for window in [
            "groovy",
            "hey groovy",
            "groovy dracula by",
            "groovy could you",
            "and then groovy play",
            "shemash play",
        ] {
            assert!(parser().wakes_on(window), "should wake on {window:?}");
        }
    }

    /// And not on a wake word that is only part of what someone is saying.
    #[test]
    fn the_gate_does_not_wake_on_a_wake_word_in_conversation() {
        for window in [
            "that is so groovy",
            "i told groovy to",
            "the groove in this",
            "groove is in the heart",
            "shemash",
        ] {
            assert!(!parser().wakes_on(window), "should not wake on {window:?}");
        }
    }

    /// Whatever the grammar takes as a command, the gate must have woken on.
    /// Otherwise the larger model never hears it.
    #[test]
    fn the_gate_wakes_on_everything_the_grammar_accepts() {
        let parser = parser();
        for heard in [
            "Groovy, play Dracula by Tame Impala.",
            "Groovy, Dracula by Tame Impala",
            "hey groovy could you please play bohemian rhapsody",
            "okay so groovy play dracula",
            "shemash play dracula",
            "Groovy.",
        ] {
            assert_ne!(
                parser.command(heard),
                Err(ParseMiss::NoPlayVerb),
                "{heard:?} should be a command"
            );
            assert!(parser.wakes_on(heard), "the gate should wake on {heard:?}");
        }
    }
}
