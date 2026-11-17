use super::*;

/// Calls a frame voiced when its first sample is non-zero, so a test says
/// exactly which frames are speech.
pub(crate) struct Marked;

impl VoiceDetector for Marked {
    fn score(&mut self, frame: &[f32]) -> f32 {
        if frame[0] != 0.0 { 1.0 } else { 0.0 }
    }
    fn reset(&mut self) {}
}

pub(crate) fn segmenter() -> Segmenter {
    Segmenter::new(Box::new(Marked))
}

pub(crate) fn voice(frames: usize) -> Vec<f32> {
    vec![0.1; frames * VAD_FRAME]
}

/// Unvoiced, but not digital silence: quiet speech the old gate dropped.
pub(crate) fn quiet(frames: usize) -> Vec<f32> {
    let mut audio = vec![0.001; frames * VAD_FRAME];
    for frame in audio.chunks_mut(VAD_FRAME) {
        frame[0] = 0.0;
    }
    audio
}

fn ended(events: Vec<Segment>) -> Option<Vec<f32>> {
    events.into_iter().find_map(|e| match e {
        Segment::Ended(audio) => Some(audio),
        Segment::Started => None,
    })
}

#[test]
fn silence_produces_nothing() {
    let mut s = segmenter();
    assert!(s.push(&quiet(500)).is_empty());
    assert!(!s.in_speech());
}

#[test]
fn speech_starts_after_the_onset_and_ends_after_the_hangover() {
    let mut s = segmenter();
    assert!(s.push(&voice(ONSET_FRAMES - 1)).is_empty());
    assert_eq!(s.push(&voice(1)), vec![Segment::Started]);
    assert!(s.push(&voice(20)).is_empty());
    assert!(s.push(&quiet(HANGOVER_FRAMES - 1)).is_empty());
    assert!(ended(s.push(&quiet(1))).is_some());
    assert!(!s.in_speech());
}

/// The fault this module exists to fix: quiet audio inside an utterance
/// used to be cut out of it.
#[test]
fn quiet_audio_inside_an_utterance_is_kept() {
    let mut s = segmenter();
    s.push(&voice(10));
    s.push(&quiet(20));
    s.push(&voice(10));
    let audio = ended(s.push(&quiet(HANGOVER_FRAMES))).expect("an utterance");
    let middle = &audio[..];
    assert!(middle.contains(&0.001), "the quiet middle was spliced out");
    assert!(audio.len() >= 40 * VAD_FRAME);
}

#[test]
fn the_lead_in_before_the_onset_is_kept() {
    let mut s = segmenter();
    s.push(&quiet(30));
    s.push(&voice(10));
    let audio = ended(s.push(&quiet(HANGOVER_FRAMES))).unwrap();
    // The onset needs ONSET_FRAMES, and the lead-in reaches back before it.
    let leading_quiet = audio.iter().take_while(|x| **x != 0.1).count();
    assert!(leading_quiet >= PREROLL_SAMPLES - ONSET_FRAMES * VAD_FRAME);
}

#[test]
fn the_trailing_hangover_is_trimmed() {
    let mut s = segmenter();
    s.push(&voice(10));
    let audio = ended(s.push(&quiet(HANGOVER_FRAMES))).unwrap();
    let trailing_quiet = audio.iter().rev().take_while(|x| **x != 0.1).count();
    assert!(trailing_quiet <= TAIL_SAMPLES, "{trailing_quiet}");
}

#[test]
fn a_click_is_not_an_utterance() {
    let mut s = segmenter();
    s.push(&voice(ONSET_FRAMES));
    assert!(ended(s.push(&quiet(HANGOVER_FRAMES))).is_none());
}

#[test]
fn endless_speech_is_cut_at_the_cap_and_continues() {
    let mut s = segmenter();
    let events = s.push(&voice(MAX_SAMPLES / VAD_FRAME + 10));
    assert!(ended(events).is_some());
    assert!(s.in_speech(), "the speaker has not stopped");
}

#[test]
fn odd_sized_pushes_are_reassembled() {
    let mut s = segmenter();
    let audio = voice(30);
    for chunk in audio.chunks(100) {
        s.push(chunk);
    }
    assert!(s.in_speech());
    assert!(ended(s.push(&quiet(HANGOVER_FRAMES))).is_some());
}

/// The real detector must at least never call digital silence speech.
#[test]
fn earshot_does_not_hear_silence() {
    let mut s = Segmenter::default();
    assert!(s.push(&vec![0.0; SAMPLE_RATE * 2]).is_empty());
}
