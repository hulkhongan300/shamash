use super::*;

fn parser() -> CommandParser {
    CommandParser::new(["groovy".to_string(), "shamash".to_string()])
}

#[test]
fn a_request_in_the_same_breath_is_understood() {
    let Understood::Request(r) =
        understand(&parser(), "Groovy, play Dracula by Tame Impala.", false)
    else {
        panic!("expected a request");
    };
    assert_eq!(r.title.as_deref(), Some("dracula"));
}

#[test]
fn play_is_not_needed_after_an_opening_wake_word() {
    assert!(matches!(
        understand(&parser(), "Shamash, Bohemian Rhapsody", false),
        Understood::Request(_)
    ));
}

#[test]
fn a_bare_wake_word_waits_for_the_request() {
    assert_eq!(
        understand(&parser(), "Groovy.", false),
        Understood::WakeOnly
    );
}

#[test]
fn an_armed_utterance_is_the_request_without_a_wake_word() {
    let Understood::Request(r) = understand(&parser(), "Dracula by Tame Impala", true) else {
        panic!("expected a request");
    };
    assert_eq!(r.artist.as_deref(), Some("tame impala"));
}

/// Things said in conversation that used to be taken as requests.
#[test]
fn conversation_is_not_a_request() {
    for heard in [
        "But I think we should go now",
        "Okay but what about lunch",
        "I told groovy to play something",
        "That's so groovy.",
    ] {
        assert!(
            matches!(understand(&parser(), heard, false), Understood::Nothing(_)),
            "{heard:?} is not a request"
        );
    }
}

#[test]
fn the_wake_model_mishearing_is_not_a_request() {
    assert!(matches!(
        understand(&parser(), "what's for lunch", false),
        Understood::Nothing(Some(ParseMiss::NoWakeWord))
    ));
}
