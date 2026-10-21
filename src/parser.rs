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

/// Wake words shorter than this must match exactly, so short words like "bot"
/// cannot match everyday speech ("not", "boy", "hot").
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
/// broken, so one edit is allowed on words long enough that a near match is
/// still unlikely to be an ordinary word.
const FUZZY_MAX_EDITS: usize = 1;

/// Whether `c` is one of the five plain vowels, which is where speech-to-text
/// substitutes when it mishears a short word ("bot" becomes "but").
fn is_vowel(c: char) -> bool {
    matches!(c, 'a' | 'e' | 'i' | 'o' | 'u')
}

/// Whether two words differ only by one vowel, which is the specific damage
/// speech-to-text does to a short wake word: "bot" comes back as "but" often
/// enough that rejecting it makes the bot feel deaf, yet "not", "boy", "hot"
/// and "top" all differ in a consonant and stay rejected.
fn differs_by_one_vowel(left: &str, right: &str) -> bool {
    // Two-letter wake words ("ut", "ot") must match exactly: otherwise the
    // ordinary words "at" and "it" wake the bot anywhere in a sentence.
    if left.chars().count() < 3 || right.chars().count() < 3 {
        return false;
    }
    let mut left = left.chars();
    let mut right = right.chars();
    let mut differences = 0;

    loop {
        match (left.next(), right.next()) {
            (None, None) => return differences == 1,
            (Some(l), Some(r)) => {
                if l != r {
                    let both_vowels = is_vowel(l) && is_vowel(r);
                    if !both_vowels {
                        return false;
                    }
                    differences += 1;
                }
            }
            _ => return false,
        }
    }
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
    /// short ones must match exactly, apart from a single swapped vowel, to
    /// avoid firing on ordinary speech.
    pub fn is_wake_word(&self, word: &str) -> bool {
        self.wake_words.iter().any(|wake| {
            word == wake
                || differs_by_one_vowel(word, wake)
                || (wake.chars().count() > EXACT_MATCH_MAX_LEN
                    && edit_distance(word, wake) <= FUZZY_MAX_EDITS)
        })
    }

    /// Requires a wake word followed by "play <something>".
    ///
    /// "play" doubles as its own wake word when it is configured as one, so
    /// "play <query>" works on its own while "bot play <query>" still does.
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
    /// and the search would then be aimed at "bot dracula" rather than
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

    /// Builds a request from text spoken after a "play" that was already heard.
    ///
    /// A natural pause inside a command splits one sentence across two
    /// utterances — "bot play", pause, "lofi hip hop" — and the second half
    /// arrives with no wake word in it. This turns that trailing half into the
    /// request the first half was waiting for.
    pub fn request_from_query(&self, text: &str) -> Option<PlayRequest> {
        build_request(&normalize(text))
    }
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
        CommandParser::new(["shamash", "bot"].into_iter().map(str::to_string))
    }

    /// A parser configured the way the shipped defaults are, with "play"
    /// acting as a wake word of its own.
    fn parser_with_play_wake_word() -> CommandParser {
        CommandParser::new(["bot", "play"].into_iter().map(str::to_string))
    }

    /// A parser configured the way the shipped defaults are, including the
    /// clipped "ut" and "ot" forms of "bot".
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
            .parse("bot play despacito")
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
        let parser = CommandParser::new(["bot", "play", "plate", "lay"].map(str::to_string));
        let req = parser.parse("late look at me. come on").unwrap();
        assert_eq!(req.query, "look at me");
    }

    /// A lone misheard verb must not arm the "next utterance is the title"
    /// path; only an exact "play" pause does that.
    #[test]
    fn a_lone_misheard_play_does_not_arm_the_next_utterance() {
        let parser = CommandParser::new(["bot", "play", "plate", "lay"].map(str::to_string));
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
        assert_eq!(parser().parse("bot play come on").unwrap().query, "come on");
        assert_eq!(
            parser().parse("bot play welcome on").unwrap().query,
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
        let req = parser().parse("bot play despacito").unwrap();
        assert_eq!(req.query, "despacito");
        assert_eq!(req.title, None);
        assert_eq!(req.artist, None);
    }

    #[test]
    fn empty_request_after_play_is_rejected() {
        assert!(parser().parse("bot play").is_none());
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
            .parse("BOT: PLAY \u{2019}talking\u{2019} BY Sade!")
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
            "bot play despacito",
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
        assert!(words.contains("bot"));
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

    #[test]
    fn short_wake_words_do_not_match_ordinary_speech() {
        let parser = parser();
        for heard in [
            "not play despacito",
            "boy play despacito",
            "hot play despacito",
            "top play despacito",
            "i will not play despacito today",
        ] {
            assert_eq!(
                parser.parse_with_reason(heard).err(),
                Some(ParseMiss::NoWakeWord),
                "should reject {heard:?}"
            );
        }
    }

    #[test]
    fn short_wake_word_tolerates_one_swapped_vowel() {
        let parser = parser();
        for heard in [
            "but play despacito",
            "Bot play despacito",
            "bat play despacito",
            "but play despacito",
        ] {
            let req = parser.parse(heard).unwrap_or_else(|| panic!("{heard:?}"));
            assert_eq!(req.query, "despacito", "for {heard:?}");
        }
    }

    #[test]
    fn vowel_substitution_needs_a_single_vowel_difference() {
        assert!(differs_by_one_vowel("but", "bot"));
        assert!(differs_by_one_vowel("shamash", "shomash"));
        assert!(!differs_by_one_vowel("bot", "bot"));
        assert!(!differs_by_one_vowel("not", "bot"));
        assert!(!differs_by_one_vowel("boy", "bot"));
        assert!(!differs_by_one_vowel("hot", "bot"));
        assert!(!differs_by_one_vowel("but", "bots"));
        assert!(!differs_by_one_vowel("but", "bu"));
        assert!(!differs_by_one_vowel("at", "ut"));
        assert!(!differs_by_one_vowel("it", "ot"));
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
                .strip_command("bot dracula by tame impala")
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
            "bot play dracula by tame impala",
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
}
