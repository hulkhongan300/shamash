//! Cuts one speaker's audio into utterances.
//!
//! A neural VAD ([`earshot`]) finds the start and end, and every sample in
//! between is kept (plus a lead-in): the old RMS gate spliced out quiet
//! consonants and pauses, which glued words together for the model.

use std::collections::VecDeque;

/// Sample rate the detector and the models expect.
pub const SAMPLE_RATE: usize = 16_000;

/// 16 ms at 16 kHz, the frame size earshot takes.
pub const VAD_FRAME: usize = 256;

const VOICE_THRESHOLD: f32 = 0.5;

/// 48 ms: one voiced frame is too easy to get from a click or cough.
const ONSET_FRAMES: usize = 3;

/// 640 ms: survives the pause after "groovy, play" yet acts promptly.
const HANGOVER_FRAMES: usize = 40;

/// 300 ms lead-in: the first word starts while the detector is still unsure.
const PREROLL_SAMPLES: usize = SAMPLE_RATE * 3 / 10;

/// 250 ms of trailing silence kept; the rest of the hangover is trimmed.
const TAIL_SAMPLES: usize = SAMPLE_RATE / 4;

/// 15 s cap; longer speech continues as a new utterance.
const MAX_SAMPLES: usize = SAMPLE_RATE * 15;

/// Utterances with fewer voiced frames (96 ms) are dropped.
const MIN_VOICED_FRAMES: usize = 6;

/// Scores a frame as voice (1.0) or not (0.0); a trait so tests can fake it.
pub trait VoiceDetector: Send {
    fn score(&mut self, frame: &[f32]) -> f32;
    fn reset(&mut self);
}

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
    Started,
    /// The whole utterance.
    Ended(Vec<f32>),
}

/// Utterance segmentation for one speaker.
pub struct Segmenter {
    detector: Box<dyn VoiceDetector>,
    /// Leftover samples short of a whole frame.
    pending: Vec<f32>,
    preroll: VecDeque<f32>,
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

    pub fn in_speech(&self) -> bool {
        self.in_speech
    }

    /// The utterance so far, lead-in included. Empty while idle.
    pub fn current(&self) -> &[f32] {
        &self.utterance
    }

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

    /// Feeds samples of any length; returns the resulting events in order.
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

    /// `still_speaking`: cut by the length cap, so a fresh utterance starts.
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
#[path = "tests/segment.rs"]
pub(crate) mod tests;
