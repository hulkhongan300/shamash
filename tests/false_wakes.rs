//! What people say to each other in the voice channel, which must not wake the
//! bot, next to what they say to it, which must.
//!
//! Every sentence in [`CONVERSATION`] once woke the bot, or would with the
//! shipped wake words: with "bot" as a wake word, "but" matched it, and a
//! sentence that opened with "but" ("but I think we should go now") was
//! searched for as a song. Both the gate, which plays the tone and lowers the
//! music, and the grammar, which decides there is a request, are checked,
//! because a false wake that only plays the tone is still the bot butting in.
//!
//! These run against [`DEFAULT_WAKE_WORDS`], so a change to the defaults has
//! to keep them passing too.

use shamash::config::DEFAULT_WAKE_WORDS;
use shamash::parser::CommandParser;
use shamash::pipeline::{Understood, understand};

fn parser() -> CommandParser {
    CommandParser::new(DEFAULT_WAKE_WORDS.iter().map(|w| w.to_string()))
}

/// Said in conversation. Neither the gate nor the grammar may take any of it.
const CONVERSATION: &[&str] = &[
    // "but" and the other one-vowel neighbours of "bot".
    "But I think we should go now",
    "but yeah",
    "Okay but what about lunch",
    "Hey, but listen to this",
    "but play that again",
    "I bet you can't",
    "that bit was funny",
    "it was a bat",
    // Talking about the bot, not to it.
    "I told the bot to play something",
    "I told groovy to play something",
    "you have to ask groovy and then play it",
    // The wake word as an ordinary word.
    "that's so groovy",
    "this is a groovy song",
    "the groovy part is the bass line",
    "let's get groovy tonight",
    "we should play groovy music",
    // One letter from the wake word.
    "this song has a great groove",
    "groove is in the heart",
    "Groove.",
    // No wake word at all.
    "can we play some games later",
    "play it cool",
    "Could you play that part again?",
    "can you put on your headphones",
    // The wake word, but no way to ask for a song after it.
    "Groovy, Dracula by Tame Impala",
    "Shamash, play Dracula",
    "groovy late look at me",
    "that's so groovy, can you believe it",
    "I told groovy to put on something",
];

/// Said to the bot. Each must wake the gate and be a request.
const REQUESTS: &[(&str, &str)] = &[
    (
        "Groovy, play Dracula by Tame Impala.",
        "dracula by tame impala",
    ),
    ("okay so groovy, play Dracula", "dracula"),
    (
        "Hey groovy, could you play Bohemian Rhapsody?",
        "bohemian rhapsody",
    ),
    ("Groovy please play Bohemian Rhapsody", "bohemian rhapsody"),
    ("Groovy, put on Dracula", "dracula"),
];

#[test]
fn conversation_does_not_wake_the_bot() {
    let parser = parser();
    for said in CONVERSATION {
        assert!(
            !parser.wakes_on(said),
            "the gate woke on conversation: {said:?}"
        );
        assert!(
            matches!(understand(&parser, said, false), Understood::Nothing(_)),
            "taken as a request: {said:?} -> {:?}",
            understand(&parser, said, false)
        );
    }
}

#[test]
fn a_request_to_the_bot_is_heard() {
    let parser = parser();
    for (said, query) in REQUESTS {
        assert!(parser.wakes_on(said), "the gate missed {said:?}");
        match understand(&parser, said, false) {
            Understood::Request(request) => assert_eq!(request.query, *query, "for {said:?}"),
            other => panic!("{said:?} should be a request, got {other:?}"),
        }
    }
}

#[test]
fn groovy_play_on_its_own_waits_for_the_request() {
    let parser = parser();
    for said in [
        "Groovy, play.",
        "hey groovy play",
        "Groovy, could you play?",
    ] {
        assert!(parser.wakes_on(said), "the gate missed {said:?}");
        assert_eq!(
            understand(&parser, said, false),
            Understood::WakeOnly,
            "for {said:?}"
        );
    }
}

/// The troubleshooting guide's example of a false wake: no "play" after
/// "groovy", so neither the gate nor the grammar takes it.
#[test]
fn the_documented_false_wake_is_not_a_request() {
    let said = "I mean the whole set was groovy, man.";
    assert!(!parser().wakes_on("groovy, man."));
    assert!(matches!(
        understand(&parser(), said, false),
        Understood::Nothing(_)
    ));
}
