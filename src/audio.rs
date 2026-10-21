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

/// Root-mean-square level of one frame, in the range 0.0..=1.0.
///
/// This is the true RMS, not the mean square. The distinction matters for
/// voice activity detection: speech oscillates around zero, so its mean square
/// is roughly the square of its RMS and reads far quieter than a loud signal of
/// the same peak level would. Thresholding the mean square therefore demands a
/// much higher *amplitude* than the number suggests — enough to miss ordinary
/// conversation entirely.
#[must_use]
pub fn rms(frame: &[f32]) -> f32 {
    let mean_square = frame.iter().map(|s| s * s).sum::<f32>() / frame.len() as f32;
    mean_square.sqrt()
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

    /// One 20 ms frame at 16 kHz.
    const FRAME: usize = 320;
    const RATE: f32 = 16_000.0;

    /// Scales `frame` so its [`rms`] is exactly `target`.
    fn normalize(frame: &mut [f32], target: f32) {
        let current = rms(frame);
        assert!(current > 0.0, "cannot normalise a silent frame");
        let gain = target / current;
        for sample in frame.iter_mut() {
            *sample *= gain;
        }
    }

    /// One cycle-and-a-bit of a pure sine at `frequency`, `amplitude` peak.
    fn tone(frequency: f32, amplitude: f32) -> Vec<f32> {
        (0..FRAME)
            .map(|i| {
                let t = i as f32 / RATE;
                amplitude * (2.0 * std::f32::consts::PI * frequency * t).sin()
            })
            .collect()
    }

    /// A 20 ms frame of speech-like audio at `target_rms`: a low fundamental
    /// with a formant on top of it, so the signal oscillates around zero the
    /// way a voice does.
    ///
    /// A constant frame such as `vec![0.5; 320]` cannot stand in for speech
    /// here. It never changes sign, so its mean square equals its RMS instead
    /// of half of it, and it hides any bug in the level metric.
    fn raw_speech_frame() -> Vec<f32> {
        let fundamental = tone(180.0, 1.0);
        let formant = tone(900.0, 0.6);
        fundamental
            .iter()
            .zip(&formant)
            .map(|(a, b)| a + b)
            .collect()
    }

    fn speech_frame(target_rms: f32) -> Vec<f32> {
        let mut frame = raw_speech_frame();
        normalize(&mut frame, target_rms);
        frame
    }

    #[test]
    fn rms_of_a_pure_tone_is_its_amplitude_over_root_two() {
        let amplitude = 0.4;
        let measured = rms(&tone(440.0, amplitude));
        let expected = amplitude / 2.0_f32.sqrt();
        assert!(
            (measured - expected).abs() < 0.01,
            "rms {measured} vs amplitude/root2 {expected}"
        );
    }

    #[test]
    fn rms_of_a_constant_frame_is_its_amplitude() {
        assert!((rms(&vec![0.5; FRAME]) - 0.5).abs() < 1e-6);
    }

    #[test]
    fn oscillating_audio_measures_far_below_its_peak() {
        // The property that made the mean-square gate miss speech: an
        // oscillating signal has a high crest factor, so its RMS sits well
        // under its peak while a constant frame's RMS equals it.
        let speech = speech_frame(0.05);
        let peak = speech.iter().map(|s| s.abs()).fold(0.0, f32::max);
        assert!(
            peak > 0.05 && rms(&speech) < peak * 0.75,
            "peak {peak}, rms {}",
            rms(&speech)
        );
    }

    #[test]
    fn quiet_speech_sits_far_below_the_old_mean_square_gate() {
        // Ordinary conversation lands around -20 dBFS RMS; a quiet voice or a
        // low microphone gain around -30 dBFS.
        let speech = speech_frame(0.05);
        let mean_square: f32 = speech.iter().map(|s| s * s).sum::<f32>() / FRAME as f32;
        assert!(
            mean_square < crate::listener::VAD_RMS_THRESHOLD,
            "mean square {mean_square} must fall under the threshold"
        );
        assert!(rms(&speech) > crate::listener::VAD_RMS_THRESHOLD);
    }
}
