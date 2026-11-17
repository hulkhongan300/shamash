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
