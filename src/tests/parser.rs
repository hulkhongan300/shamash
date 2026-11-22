use super::*;

fn parser() -> CommandParser {
    CommandParser::new(["shamash", "groovy"].into_iter().map(str::to_string))
}

#[test]
fn a_clipped_wake_word_is_not_invented_when_not_configured() {
    let bot_only = CommandParser::new(["bot".to_string()]);
    assert_eq!(
        bot_only.command("ut play despacito"),
        Err(ParseMiss::NoWakeWord)
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

/// Only the wake word and a way to ask for a song start a command.
#[test]
fn only_the_wake_word_then_play_is_a_command() {
    let parser = parser();
    for heard in [
        "groovy",
        "hey groovy",
        "groovy, dracula by tame impala",
        "hey shamash please bohemian rhapsody",
        "groovy could you dracula",
        "groovy late look at me",
        "groovy plate dracula",
        "groovy lay dracula",
        "play dracula",
    ] {
        assert!(parser.command(heard).is_err(), "{heard:?} is not a command");
        assert!(
            !parser.wakes_on(heard),
            "the gate should not wake on {heard:?}"
        );
    }
}

/// A greeting first, "could you", "please", or another verb than "play".
#[test]
fn a_request_can_be_asked_for_naturally() {
    let parser = parser();
    for (heard, query) in [
        ("Hey groovy, play Despacito.", "despacito"),
        (
            "Groovy, can you play Dracula by Tame Impala?",
            "dracula by tame impala",
        ),
        (
            "Hey groovy, could you please play Bohemian Rhapsody?",
            "bohemian rhapsody",
        ),
        ("Groovy please play Blinding Lights", "blinding lights"),
        (
            "Groovy, would you put on Hotel California?",
            "hotel california",
        ),
        ("groovy will you throw on some lofi", "some lofi"),
        // Only the asking is taken off, never the title's own words.
        ("Groovy, put on On My Way.", "on my way"),
        ("Groovy, play Please Please Please.", "please please please"),
        (
            "Groovy, play Can You Feel My Heart",
            "can you feel my heart",
        ),
    ] {
        assert!(parser.wakes_on(heard), "the gate missed {heard:?}");
        assert_eq!(
            parser.command(heard).map(|r| r.query).as_deref(),
            Ok(query),
            "for {heard:?}"
        );
    }
    assert_eq!(
        parser.command("Hey groovy, could you play?"),
        Err(ParseMiss::EmptyRequest)
    );
}

/// The same words said to somebody else, or not followed by a verb.
#[test]
fn asking_somebody_else_is_not_a_command() {
    let parser = parser();
    for heard in [
        "Could you play that part again?",
        "can you put on your headphones",
        "please play nice",
        "that's so groovy, can you believe it",
        "I told groovy to put on something",
        "groovy, would you mind",
    ] {
        assert!(!parser.wakes_on(heard), "the gate woke on {heard:?}");
        assert!(parser.command(heard).is_err(), "{heard:?} is not a command");
    }
}

/// How the wake model wrote "groovy play" for synthetic voices.
#[test]
fn the_gate_wakes_however_the_wake_model_spells_groovy_play() {
    let parser = parser();
    for heard in [
        "Ruby play Bohemian Rhapsody",
        "Rove, play Dracula by",
        "Groovey played Dracula.",
        "Provey play seventeen",
        "Review play blinding lights",
        "shemash play despacito",
    ] {
        assert!(parser.wakes_on(heard), "the gate missed {heard:?}");
    }
}

/// How the request model wrote it; there, "Ruby" is a name, not "groovy".
#[test]
fn a_command_is_heard_however_the_request_model_spells_groovy_play() {
    let parser = parser();
    for (heard, query) in [
        (
            "Roovy, play Dracula by Tame Impala.",
            "dracula by tame impala",
        ),
        ("Ruvie played Dracula.", "dracula"),
        ("Groovy played Blinding Lights.", "blinding lights"),
        ("Rovie play seventeen", "seventeen"),
    ] {
        assert!(parser.wakes_on(heard), "the gate missed {heard:?}");
        assert_eq!(parser.command(heard).map(|r| r.query).as_deref(), Ok(query));
    }
    for heard in ["Ruby, play your card.", "Robbie played the drums."] {
        assert!(parser.command(heard).is_err(), "{heard:?} is not a command");
    }
}

/// Rhymes that end differently, or lose "play"; a short wake word stays exact.
#[test]
fn conversation_that_rhymes_is_not_a_command() {
    let parser = parser();
    for heard in [
        "Rob played really well",
        "The group played at the festival",
        "Ruby plays the violin",
        "can you replay that part",
        "grab a plate and sit down",
        "I love that movie play it again",
        "make a groovy playlist",
    ] {
        assert!(!parser.wakes_on(heard), "the gate woke on {heard:?}");
        assert!(parser.command(heard).is_err(), "{heard:?} is not a command");
    }
    let bot = CommandParser::new(["bot".to_string()]);
    assert!(!bot.wakes_on("but play it again"));
    assert!(bot.wakes_on("bot play it again"));
}

#[test]
fn a_trailing_come_on_is_dropped() {
    assert_eq!(
        parser()
            .parse("groovy play look at me. come on")
            .unwrap()
            .query,
        "look at me"
    );
}

/// "come on" and "welcome on" are plausible titles, never eaten.
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

/// Last " by " first: "us and them by pink floyd" needs a two-word artist.
#[test]
fn the_last_by_stays_the_primary_guess() {
    let req = parser()
        .parse("shamash play stand by me by ben e king")
        .unwrap();
    assert_eq!(req.title.as_deref(), Some("stand by me"));
    assert_eq!(req.artist.as_deref(), Some("ben e king"));
    assert_eq!(req.query, "stand by me by ben e king");
}

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
    // A leading "by" produces no split.
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

/// "By the Way" needs the whole-phrase reading even when a split exists.
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

/// Avoids sending the resolver the same query twice.
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
        parser.command("what is the weather"),
        Err(ParseMiss::NoWakeWord)
    );
    assert_eq!(
        parser.command("shamash what is the weather"),
        Err(ParseMiss::NoPlayVerb)
    );
    assert_eq!(parser.command("shamash play"), Err(ParseMiss::EmptyRequest));
    assert!(parser.command("shamash play despacito").is_ok());
}

#[test]
fn exposes_configured_wake_words() {
    let parser = parser();
    let words = parser.wake_words();
    assert!(words.contains("shamash"));
    assert!(words.contains("groovy"));
}

#[test]
fn a_capitalised_wake_word_is_the_same_word() {
    let parser = CommandParser::new(["Groovy".to_string()]);
    assert!(parser.parse("groovy play despacito").is_some());
}

/// "bot" once matched one swapped vowel, so every "but" woke it.
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
            parser.command(heard).err(),
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
        parser.command("chameleon play despacito").err(),
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

/// The rewriter returns whole sentences, wake word included.
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
    let parser = parser();
    for transcript in [
        "groovy play dracula by tame impala",
        "shamash play us and them by pink floyd",
        "groovy play",
    ] {
        assert_eq!(
            parser.strip_command(transcript),
            parser.parse(transcript).map(|r| r.query),
            "for {transcript:?}"
        );
    }
}

#[test]
fn command_still_understands_play() {
    assert_eq!(
        parser().command("groovy play dracula").unwrap().query,
        "dracula"
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
}

/// A mid-sentence "play" is describing a request, not making one.
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

#[test]
fn a_later_wake_word_is_still_heard() {
    assert_eq!(
        parser()
            .command("that groove was nice groovy play dracula")
            .unwrap()
            .query,
        "dracula"
    );
    // Arms even after a wake word that was only conversation.
    assert_eq!(
        parser().command("the groove so groovy play"),
        Err(ParseMiss::EmptyRequest)
    );
}

#[test]
fn a_bare_wake_word_and_play_is_an_empty_request() {
    assert_eq!(parser().command("Groovy."), Err(ParseMiss::NoPlayVerb));
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
            "Groovy, play the album Thriller by Michael Jackson.",
            "thriller by michael jackson",
        ),
    ] {
        assert_eq!(album_of(spoken).as_deref(), Some(album), "for {spoken:?}");
    }
}

/// Without "album", the catalog decides.
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
    assert!(parser().wakes_on("Hey, GROOVY! Play"));
    assert!(!parser().wakes_on("not now"));
}

#[test]
fn the_gate_does_not_wake_on_a_wake_word_in_conversation() {
    for window in [
        "that is so groovy",
        "i told groovy to",
        "the groove in this",
        "groove is in the heart",
    ] {
        assert!(!parser().wakes_on(window), "should not wake on {window:?}");
    }
}

/// Otherwise the larger model never hears it.
#[test]
fn the_gate_wakes_on_everything_the_grammar_accepts() {
    let parser = parser();
    for heard in [
        "Groovy, play Dracula by Tame Impala.",
        "okay so groovy play dracula",
        "and then groovy play",
    ] {
        assert_ne!(
            parser.command(heard),
            Err(ParseMiss::NoPlayVerb),
            "{heard:?} should be a command"
        );
        assert!(parser.wakes_on(heard), "the gate should wake on {heard:?}");
    }
}

fn queries(query: &str, every: bool) -> Vec<String> {
    songs(query, every).into_iter().map(|r| r.query).collect()
}

#[test]
fn and_play_or_and_then_always_starts_another_song() {
    for query in [
        "dracula by tame impala and play bohemian rhapsody by queen",
        "dracula by tame impala and then bohemian rhapsody by queen",
        "dracula by tame impala and also bohemian rhapsody by queen",
        "dracula by tame impala and then play bohemian rhapsody by queen",
        "dracula by tame impala and could you please put on bohemian rhapsody by queen",
    ] {
        assert_eq!(
            queries(query, false),
            ["dracula by tame impala", "bohemian rhapsody by queen"],
            "{query:?}"
        );
    }
}

#[test]
fn a_bare_and_splits_only_when_asked() {
    assert_eq!(queries("me and bobby mcgee", false), ["me and bobby mcgee"]);
    assert_eq!(
        queries("dracula and bohemian rhapsody and hey jude", true),
        ["dracula", "bohemian rhapsody", "hey jude"]
    );
}

#[test]
fn an_explicit_split_keeps_and_inside_names() {
    assert_eq!(
        queries(
            "mrs robinson by simon and garfunkel and play dracula",
            false
        ),
        ["mrs robinson by simon and garfunkel", "dracula"]
    );
}

#[test]
fn a_dangling_and_adds_no_empty_song() {
    assert_eq!(queries("dracula and then", false), ["dracula"]);
    assert_eq!(queries("dracula and", true), ["dracula"]);
}
