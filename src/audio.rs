/// Re-samples audio between two rates for Whisper's 16 kHz front end.
#[derive(Debug, Clone)]
pub struct Resampler {
    input_rate: u32,
    output_rate: u32,
}

impl Resampler {
    pub fn new(input_rate: u32, output_rate: u32) -> Self {
        Self {
            input_rate,
            output_rate,
        }
    }

    /// Re-samples `input`.
    ///
    /// Integer ratios use an averaging (box) filter that also acts as a cheap
    /// anti-aliasing low-pass; non-integer ratios fall back to linear
    /// interpolation.
    pub fn resample(&self, input: &[f32]) -> Vec<f32> {
        if self.input_rate == self.output_rate {
            return input.to_vec();
        }
        if self.input_rate.is_multiple_of(self.output_rate) {
            let factor = (self.input_rate / self.output_rate) as usize;
            input
                .chunks_exact(factor)
                .map(|chunk| chunk.iter().sum::<f32>() / chunk.len() as f32)
                .collect()
        } else {
            self.resample_linear(input)
        }
    }

    fn resample_linear(&self, input: &[f32]) -> Vec<f32> {
        let ratio = self.input_rate as f32 / self.output_rate as f32;
        let out_len = (input.len() as f32 / ratio).floor() as usize;
        let mut out = Vec::with_capacity(out_len);
        let mut pos = 0.0f32;
        while out.len() < out_len && pos < input.len() as f32 {
            let idx = pos as usize;
            let frac = pos - idx as f32;
            let a = input[idx];
            let b = input.get(idx + 1).copied().unwrap_or(a);
            out.push(a + frac * (b - a));
            pos += ratio;
        }
        out
    }
}

/// Voice-activity detection: accumulates speech and yields complete
/// utterances on silence gaps.
#[derive(Debug)]
pub struct VadBuffer {
    frame_samples: usize,
    threshold: f32,
    gap_frames: usize,
    max_frames: usize,
    buffer: Vec<f32>,
    silent_streak: usize,
    in_utterance: bool,
}

impl VadBuffer {
    /// `frame_samples` is the size of one pushed frame (e.g. 20 ms at the
    /// pipelined sample rate). `threshold` is the RMS below which a frame
    /// counts as silence. `gap_frames` silence frames end an utterance.
    /// `max_frames` forces a flush to bound utterance length.
    pub fn new(frame_samples: usize, threshold: f32, gap_frames: usize, max_frames: usize) -> Self {
        Self {
            frame_samples,
            threshold,
            gap_frames,
            max_frames,
            buffer: Vec::new(),
            silent_streak: 0,
            in_utterance: false,
        }
    }

    /// Feeds one frame; returns samples of a completed utterance, if any.
    ///
    /// # Panics
    ///
    /// Panics if `frame` is not exactly `frame_samples` long.
    pub fn push(&mut self, frame: &[f32]) -> Vec<f32> {
        assert_eq!(
            frame.len(),
            self.frame_samples,
            "VAD frame size mismatch: got {}, expected {}",
            frame.len(),
            self.frame_samples
        );

        let rms = frame.iter().map(|s| s * s).sum::<f32>() / frame.len() as f32;
        if rms >= self.threshold {
            self.buffer.extend_from_slice(frame);
            self.in_utterance = true;
            self.silent_streak = 0;
            let frame_count = self.buffer.len() / self.frame_samples;
            if frame_count >= self.max_frames {
                return self.flush();
            }
        } else if self.in_utterance {
            self.silent_streak += 1;
            if self.silent_streak >= self.gap_frames {
                return self.flush();
            }
        }
        Vec::new()
    }

    fn flush(&mut self) -> Vec<f32> {
        let samples = std::mem::take(&mut self.buffer);
        self.in_utterance = false;
        self.silent_streak = 0;
        samples
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn integer_ratio_averages_thirds() {
        let resampler = Resampler::new(48_000, 16_000);
        let input: Vec<f32> = (0..12).map(|i| i as f32).collect();
        let out = resampler.resample(&input);
        assert_eq!(out, vec![1.0, 4.0, 7.0, 10.0]);
    }

    #[test]
    fn same_rate_passes_through() {
        let resampler = Resampler::new(48_000, 48_000);
        let input = vec![0.25, 0.5, -0.75];
        assert_eq!(resampler.resample(&input), input);
    }

    #[test]
    fn non_integer_ratio_interpolates() {
        let resampler = Resampler::new(44_100, 16_000);
        let input: Vec<f32> = (0..10).map(|i| i as f32).collect();
        let out = resampler.resample(&input);
        assert_eq!(out.len(), 3);
        assert!((out[0]).abs() < 0.01);
        assert!((out[1] - 2.756).abs() < 0.01);
        assert!((out[2] - 5.512).abs() < 0.01);
    }

    #[test]
    fn constant_signal_is_unchanged() {
        let resampler = Resampler::new(48_000, 16_000);
        let input = vec![0.5; 900];
        let out = resampler.resample(&input);
        assert_eq!(out.len(), 300);
        assert!(out.iter().all(|s| (s - 0.5).abs() < 1e-6));
    }

    fn loud_frame() -> Vec<f32> {
        vec![0.5; 320]
    }

    fn silent_frame() -> Vec<f32> {
        vec![0.0; 320]
    }

    #[test]
    fn flushes_utterance_on_silence_gap() {
        let mut vad = VadBuffer::new(320, 0.01, 2, 100);
        for _ in 0..5 {
            assert!(vad.push(&loud_frame()).is_empty());
        }
        assert!(vad.push(&silent_frame()).is_empty());
        let utterance = vad.push(&silent_frame());
        assert_eq!(utterance.len(), 5 * 320);
    }

    #[test]
    fn leading_silence_is_discarded() {
        let mut vad = VadBuffer::new(320, 0.01, 2, 100);
        for _ in 0..10 {
            assert!(vad.push(&silent_frame()).is_empty());
        }
        for _ in 0..3 {
            assert!(vad.push(&loud_frame()).is_empty());
        }
        assert!(vad.push(&silent_frame()).is_empty());
        let utterance = vad.push(&silent_frame());
        assert_eq!(utterance.len(), 3 * 320);
        assert!(vad.push(&silent_frame()).is_empty());
    }

    #[test]
    fn overflow_flushes_mid_speech() {
        let mut vad = VadBuffer::new(320, 0.01, 2, 4);
        for _ in 0..3 {
            assert!(vad.push(&loud_frame()).is_empty());
        }
        let utterance = vad.push(&loud_frame());
        assert_eq!(utterance.len(), 4 * 320);
    }

    #[test]
    #[should_panic(expected = "frame size mismatch")]
    fn wrong_frame_size_panics() {
        let mut vad = VadBuffer::new(320, 0.01, 2, 100);
        let _ = vad.push(&vec![0.5; 128]);
    }
}
