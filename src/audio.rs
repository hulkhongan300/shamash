/// Re-samples audio between two rates for the models' 16 kHz front end.
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

    /// Re-samples `input`: box filter (cheap anti-aliasing) for integer ratios, else linear.
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

/// Root-mean-square level of one frame, in 0.0..=1.0.
///
/// True RMS, not mean square: gating on mean square misses ordinary speech.
#[must_use]
pub fn rms(frame: &[f32]) -> f32 {
    if frame.is_empty() {
        return 0.0;
    }
    let mean_square = frame.iter().map(|s| s * s).sum::<f32>() / frame.len() as f32;
    mean_square.sqrt()
}

#[cfg(test)]
#[path = "tests/audio.rs"]
mod tests;
