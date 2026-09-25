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
}

/// Wake words shorter than this must match exactly, so short words like "bot"
/// cannot match everyday speech ("not", "boy", "hot").
const EXACT_MATCH_MAX_LEN: usize = 4;

/// Single-character mistakes tolerated in longer wake words.
///
/// Speech-to-text reliably mangles a proper name — "shamash" can come back as
/// "shemash", "samash" or "shammash" — and rejecting those makes the bot feel
/// broken, so one edit is allowed on words long enough that a near match is
/// still unlikely to be an ordinary word.
const FUZZY_MAX_EDITS: usize = 1;

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
#[derive(Debug)]
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
    /// short ones must match exactly to avoid firing on ordinary speech.
    pub fn is_wake_word(&self, word: &str) -> bool {
        self.wake_words.iter().any(|wake| {
            word == wake
                || (wake.chars().count() > EXACT_MATCH_MAX_LEN
                    && edit_distance(word, wake) <= FUZZY_MAX_EDITS)
        })
    }

    /// Requires a wake word followed by "play <something>".
    ///
    /// Recognizes "play <title> by <artist>" (the artist gets the search bias)
    /// and falls back to the raw text after "play" as the query.
    pub fn parse(&self, transcript: &str) -> Option<PlayRequest> {
        self.parse_with_reason(transcript).ok()
    }

    /// Like [`Self::parse`], but reports why the transcript was rejected.
    pub fn parse_with_reason(&self, transcript: &str) -> Result<PlayRequest, ParseMiss> {
        let text = normalize(transcript);
        let words: Vec<&str> = text.split(' ').collect();

        let wake_idx = words
            .iter()
            .position(|w| self.is_wake_word(w))
            .ok_or(ParseMiss::NoWakeWord)?;
        let play_at = words[wake_idx + 1..]
            .iter()
            .position(|w| *w == "play")
            .map(|offset| wake_idx + 1 + offset)
            .ok_or(ParseMiss::NoPlayVerb)?;

        let rest = words[play_at + 1..].join(" ");
        if rest.is_empty() {
            return Err(ParseMiss::EmptyRequest);
        }

        let (title, artist) = split_artist(&rest);
        let query = match (&title, &artist) {
            (Some(title), Some(artist)) => format!("{title} by {artist}"),
            _ => rest,
        };

        Ok(PlayRequest {
            query,
            title,
            artist,
        })
    }
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

fn split_artist(rest: &str) -> (Option<String>, Option<String>) {
    let Some(idx) = rest.rfind(" by ") else {
        return (None, None);
    };
    let (title, artist) = rest.split_at(idx);
    let title = title.trim();
    let artist = artist.trim_start_matches(" by ").trim();
    if title.is_empty() || artist.is_empty() {
        return (None, None);
    }
    (Some(title.to_string()), Some(artist.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parser() -> CommandParser {
        CommandParser::new(["shamash", "bot"].into_iter().map(str::to_string))
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
    fn ignores_wake_words_that_are_too_far_off() {
        let parser = parser();
        assert_eq!(
            parser.parse_with_reason("chameleon play despacito").err(),
            Some(ParseMiss::NoWakeWord)
        );
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
