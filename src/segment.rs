//! Cuts one speaker's audio into utterances.
//!
//! This replaces a fixed RMS gate that kept only the frames louder than
//! -34 dBFS. That gate had two faults that both cost words. A quiet speaker
//! never crossed it at all. Worse, the frames it *dropped* inside an utterance
//! were the quiet ones: the soft consonants, the ends of words, the pauses
//! between them. The model was handed speech with those spliced out, which
//! reads to it as words glued together.
//!
//! Here a small neural voice detector ([`earshot`]) decides where speech
//! starts and stops, and **every** sample between those points is kept: a
//! short lead-in from before the onset, so the first syllable is not clipped,
//! and the quiet stretches inside the utterance, so nothing is spliced.

use std::collections::VecDeque;

/// Sample rate the detector and the models expect.
pub const SAMPLE_RATE: usize = 16_000;

/// Samples per detector frame: 16 ms at 16 kHz, which is what earshot takes.
pub const VAD_FRAME: usize = 256;

/// Score at or above which a frame counts as voice.
const VOICE_THRESHOLD: f32 = 0.5;

/// Consecutive voiced frames that start an utterance: 3 x 16 ms.
///
/// A single voiced frame is too easy to get from a click or a cough.
const ONSET_FRAMES: usize = 3;

/// Consecutive unvoiced frames that end an utterance: 40 x 16 ms = 640 ms.
///
/// Long enough to survive the pause between "bot, play" and the title, short
/// enough that the request is acted on promptly once the speaker stops.
const HANGOVER_FRAMES: usize = 40;

/// Audio kept from before the onset: 300 ms.
///
/// The detector needs a few frames to be sure, and the start of the first word
/// is in those frames.
const PREROLL_SAMPLES: usize = SAMPLE_RATE * 3 / 10;

/// Silence kept after the last voiced frame: 250 ms. The rest of the hangover
/// is trimmed off, because it is only there to decide that speech ended.
const TAIL_SAMPLES: usize = SAMPLE_RATE / 4;

/// Hard cap on one utterance: 15 s. Speech that runs longer is cut there and
/// carries on as a new utterance.
const MAX_SAMPLES: usize = SAMPLE_RATE * 15;

/// Utterances with fewer voiced frames than this are dropped: 6 x 16 ms.
const MIN_VOICED_FRAMES: usize = 6;

/// Scores one frame as voice (1.0) or not (0.0).
///
/// A trait so the segmenter's own logic can be tested with a detector whose
/// answers are known, rather than with recordings.
pub trait VoiceDetector: Send {
    fn score(&mut self, frame: &[f32]) -> f32;
    fn reset(&mut self);
}

/// The production detector.
pub struct Earshot(Box<earshot::Detector>);

impl Default for Earshot {
    fn default() -> Self {
        Self(earshot::Detector::default_boxed())
    }
}

impl VoiceDetector for Earshot {
    fn score(&mut self, frame: &[f32]) -> f32 {
        self.0.predict_f32(frame)
    }

    fn reset(&mut self) {
        self.0.reset();
    }
}

/// What a push into the segmenter produced.
#[derive(Debug, PartialEq)]
pub enum Segment {
    /// Speech began.
    Started,
    /// Speech ended; this is the whole utterance.
    Ended(Vec<f32>),
}

/// Utterance segmentation for one speaker.
pub struct Segmenter {
    detector: Box<dyn VoiceDetector>,
    /// Samples not yet making up a whole detector frame.
    pending: Vec<f32>,
    /// The most recent audio while idle, for the lead-in.
    preroll: VecDeque<f32>,
    /// The utterance in progress.
    utterance: Vec<f32>,
    in_speech: bool,
    voiced_run: usize,
    silent_run: usize,
    voiced_total: usize,
    /// Length of `utterance` at the end of the last voiced frame.
    last_voiced_end: usize,
}

impl Default for Segmenter {
    fn default() -> Self {
        Self::new(Box::new(Earshot::default()))
    }
}

impl Segmenter {
    pub fn new(detector: Box<dyn VoiceDetector>) -> Self {
        Self {
            detector,
            pending: Vec::with_capacity(VAD_FRAME * 2),
            preroll: VecDeque::with_capacity(PREROLL_SAMPLES + VAD_FRAME),
            utterance: Vec::new(),
            in_speech: false,
            voiced_run: 0,
            silent_run: 0,
            voiced_total: 0,
            last_voiced_end: 0,
        }
    }

    /// Whether an utterance is in progress.
    pub fn in_speech(&self) -> bool {
        self.in_speech
    }

    /// The utterance so far, lead-in included. Empty while idle.
    pub fn current(&self) -> &[f32] {
        &self.utterance
    }

    /// Forgets everything, as if the speaker had just arrived.
    pub fn reset(&mut self) {
        self.detector.reset();
        self.pending.clear();
        self.preroll.clear();
        self.utterance.clear();
        self.in_speech = false;
        self.voiced_run = 0;
        self.silent_run = 0;
        self.voiced_total = 0;
        self.last_voiced_end = 0;
    }

    /// Feeds any number of samples and returns what they caused, in order.
    pub fn push(&mut self, samples: &[f32]) -> Vec<Segment> {
        self.pending.extend_from_slice(samples);
        let mut out = Vec::new();
        let mut start = 0;
        while self.pending.len() - start >= VAD_FRAME {
            let frame: Vec<f32> = self.pending[start..start + VAD_FRAME].to_vec();
            start += VAD_FRAME;
            self.frame(&frame, &mut out);
        }
        self.pending.drain(..start);
        out
    }

    fn frame(&mut self, frame: &[f32], out: &mut Vec<Segment>) {
        let voiced = self.detector.score(frame) >= VOICE_THRESHOLD;

        if !self.in_speech {
            self.preroll.extend(frame.iter().copied());
            while self.preroll.len() > PREROLL_SAMPLES {
                self.preroll.pop_front();
            }
            self.voiced_run = if voiced { self.voiced_run + 1 } else { 0 };
            if self.voiced_run >= ONSET_FRAMES {
                self.in_speech = true;
                self.utterance = self.preroll.drain(..).collect();
                self.voiced_total = self.voiced_run;
                self.silent_run = 0;
                self.last_voiced_end = self.utterance.len();
                out.push(Segment::Started);
            }
            return;
        }

        self.utterance.extend_from_slice(frame);
        if voiced {
            self.voiced_total += 1;
            self.silent_run = 0;
            self.last_voiced_end = self.utterance.len();
        } else {
            self.silent_run += 1;
        }

        if self.silent_run >= HANGOVER_FRAMES {
            self.close(out, false);
        } else if self.utterance.len() >= MAX_SAMPLES {
            self.close(out, true);
        }
    }

    /// Ends the utterance. `still_speaking` is the length cap cutting speech
    /// that has not stopped, which carries on as a fresh utterance.
    fn close(&mut self, out: &mut Vec<Segment>, still_speaking: bool) {
        let mut audio = std::mem::take(&mut self.utterance);
        if !still_speaking {
            audio.truncate((self.last_voiced_end + TAIL_SAMPLES).min(audio.len()));
        }
        let enough = self.voiced_total >= MIN_VOICED_FRAMES;

        self.preroll.clear();
        self.silent_run = 0;
        self.voiced_total = 0;
        self.last_voiced_end = 0;
        self.in_speech = still_speaking;
        self.voiced_run = 0;

        if enough {
            out.push(Segment::Ended(audio));
        }
        if still_speaking {
            out.push(Segment::Started);
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
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
}
