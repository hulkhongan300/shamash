use super::*;
use crate::segment::tests::{Marked, quiet, voice};

/// A model that answers every check with the same text.
struct FixedText(&'static str);

impl Transcriber for FixedText {
    fn sample_rate(&self) -> u32 {
        16_000
    }
    fn transcribe(&self, _samples: &[f32]) -> anyhow::Result<String> {
        Ok(self.0.to_string())
    }
}

fn parser() -> CommandParser {
    CommandParser::new(["shamash".to_string(), "groovy".to_string()])
}

fn gate(armed: &Armed) -> WakeGate {
    WakeGate::with_detector(Arc::new(FixedText("")), parser(), armed.clone(), || {
        Box::new(Marked)
    })
}

fn checks(steps: &[Step]) -> usize {
    steps
        .iter()
        .filter(|s| matches!(s, Step::Check { closing: None, .. }))
        .count()
}

#[test]
fn silence_is_never_offered_for_checking() {
    let armed = Armed::default();
    let mut gate = gate(&armed);
    assert!(gate.push(1, &quiet(300)).is_empty());
}

#[test]
fn speech_is_checked_on_a_rolling_window() {
    let armed = Armed::default();
    let mut gate = gate(&armed);
    let steps: Vec<Step> = voice(200)
        .chunks(320)
        .flat_map(|c| gate.push(1, c))
        .collect();
    // 200 x 16 ms = 3.2 s of speech.
    assert!(checks(&steps) >= 5, "{} checks", checks(&steps));
    for step in &steps {
        if let Step::Check { window, .. } = step {
            assert!(window.len() <= samples(WAKE_WINDOW));
        }
    }
}

/// Words said with the wake word must not be lost.
#[test]
fn a_woken_utterance_is_delivered_whole() {
    let armed = Armed::default();
    let mut gate = gate(&armed);
    gate.push(1, &voice(40));
    let heard = gate.transcribed(1, "groovy play dracula", None);
    assert!(matches!(heard[..], [Heard::Wake { ssrc: 1 }]));

    gate.push(1, &voice(100));
    let steps = gate.push(1, &quiet(50));
    let audio = steps
        .into_iter()
        .find_map(|s| match s {
            Step::Heard(Heard::Utterance {
                audio, woke: true, ..
            }) => Some(audio),
            _ => None,
        })
        .expect("the utterance");
    assert!(audio.len() >= 140 * crate::segment::VAD_FRAME);
}

#[test]
fn ordinary_speech_is_not_delivered() {
    let armed = Armed::default();
    let mut gate = gate(&armed);
    gate.push(1, &voice(40));
    assert!(gate.transcribed(1, "what is for lunch", None).is_empty());
    let steps = gate.push(1, &quiet(50));
    let Some(Step::Check {
        closing: Some(audio),
        window,
        ..
    }) = steps.into_iter().next()
    else {
        panic!("expected a closing check");
    };
    assert!(window.len() <= samples(WAKE_WINDOW));
    assert!(
        gate.transcribed(1, "what is for lunch", Some(audio))
            .is_empty()
    );
}

#[test]
fn a_wake_word_in_conversation_does_not_wake() {
    let armed = Armed::default();
    let mut gate = gate(&armed);
    gate.push(1, &voice(40));
    assert!(gate.transcribed(1, "that is so groovy", None).is_empty());
    assert!(gate.transcribed(1, "i told groovy to", None).is_empty());
    assert!(gate.transcribed(1, "but play it again", None).is_empty());
}

#[test]
fn a_wake_word_at_the_very_end_is_caught_by_the_closing_check() {
    let armed = Armed::default();
    let mut gate = gate(&armed);
    gate.push(1, &voice(40));
    let steps = gate.push(1, &quiet(50));
    let Some(Step::Check {
        closing: Some(audio),
        ..
    }) = steps.into_iter().next()
    else {
        panic!("expected a closing check");
    };
    let heard = gate.transcribed(1, "okay shamash play", Some(audio));
    assert!(matches!(
        heard[..],
        [
            Heard::Wake { .. },
            Heard::Utterance {
                woke: true,
                armed: false,
                ..
            }
        ]
    ));
}

#[test]
fn one_utterance_wakes_only_once() {
    let armed = Armed::default();
    let mut gate = gate(&armed);
    gate.push(1, &voice(40));
    assert_eq!(gate.transcribed(1, "groovy play", None).len(), 1);
    assert!(gate.transcribed(1, "groovy play", None).is_empty());
    assert_eq!(checks(&gate.push(1, &voice(100))), 0);
}

#[test]
fn an_armed_speaker_is_delivered_without_a_wake_word() {
    let armed = Armed::default();
    let mut gate = gate(&armed);
    armed.arm(7);
    let steps = gate.push(7, &voice(60));
    assert_eq!(checks(&steps), 0, "an armed speaker is not wake-checked");
    let steps = gate.push(7, &quiet(50));
    assert!(matches!(
        steps[..],
        [Step::Heard(Heard::Utterance {
            armed: true,
            woke: false,
            ..
        })]
    ));
    assert!(!armed.is_armed(7), "arming is consumed");
}

#[test]
fn speakers_are_kept_apart() {
    let armed = Armed::default();
    let mut gate = gate(&armed);
    gate.push(1, &voice(40));
    gate.push(2, &voice(40));
    gate.transcribed(1, "groovy play", None);
    let other = gate.push(2, &quiet(50));
    assert!(
        other
            .iter()
            .all(|s| !matches!(s, Step::Heard(Heard::Utterance { .. }))),
        "speaker 2 never said the wake word"
    );
    let first = gate.push(1, &quiet(50));
    assert!(matches!(
        first[..],
        [Step::Heard(Heard::Utterance { ssrc: 1, .. })]
    ));
}

#[test]
fn an_expired_arming_is_not_live() {
    let armed = Armed::default();
    let deadline = armed.arm(3);
    assert!(armed.expire(3, deadline));
    assert!(!armed.take(3));
    let old = Instant::now();
    armed.arm(3);
    assert!(!armed.expire(3, old));
    assert!(armed.is_armed(3));
}

#[tokio::test]
async fn the_loop_drops_audio_while_locked() {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<Tick>();
    let heard = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::clone(&heard);
    let armed = Armed::default();
    let gate = WakeGate::with_detector(
        Arc::new(FixedText("groovy play dracula")),
        parser(),
        armed,
        || Box::new(Marked),
    );
    let handle = tokio::spawn(run_wake_gate(
        gate,
        rx,
        || true,
        move |h| seen.lock().unwrap().push(format!("{h:?}").len()),
    ));
    tx.send(vec![(1, voice(100))]).unwrap();
    tx.send(vec![(1, quiet(60))]).unwrap();
    drop(tx);
    handle.await.unwrap();
    assert!(heard.lock().unwrap().is_empty());
}

#[tokio::test]
async fn the_loop_delivers_the_wake_before_the_utterance() {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<Tick>();
    let order = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::clone(&order);
    let gate = WakeGate::with_detector(
        Arc::new(FixedText("groovy play dracula")),
        parser(),
        Armed::default(),
        || Box::new(Marked),
    );
    let handle = tokio::spawn(run_wake_gate(
        gate,
        rx,
        || false,
        move |h| {
            seen.lock().unwrap().push(match h {
                Heard::Wake { .. } => "wake",
                Heard::Utterance { .. } => "utterance",
            })
        },
    ));
    for chunk in voice(100).chunks(320) {
        tx.send(vec![(1, chunk.to_vec())]).unwrap();
    }
    tx.send(vec![(1, quiet(60))]).unwrap();
    drop(tx);
    handle.await.unwrap();
    assert_eq!(order.lock().unwrap().clone(), vec!["wake", "utterance"]);
}
