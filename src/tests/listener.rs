use super::*;

#[test]
fn speakers_are_kept_apart_rather_than_mixed() {
    let tick = split_tick(
        [
            (1, Some(&[1000i16; 320][..])),
            (2, Some(&[-2000i16; 320][..])),
        ],
        [],
    );
    assert_eq!(tick.len(), 2);
    let one = tick.iter().find(|(s, _)| *s == 1).unwrap();
    let two = tick.iter().find(|(s, _)| *s == 2).unwrap();
    assert!((one.1[0] - 1000.0 / 32768.0).abs() < 1e-6);
    assert!((two.1[0] + 2000.0 / 32768.0).abs() < 1e-6);
}

/// The failure this prevents: a speaker stops transmitting, Discord sends
/// nothing at all, and their utterance never ends.
#[test]
fn a_silent_speaker_is_given_silence() {
    let tick = split_tick([], [9]);
    assert_eq!(tick, vec![(9, vec![0.0; FRAME_SAMPLES])]);
}

#[test]
fn a_lost_packet_is_silence_not_nothing() {
    let tick = split_tick([(4, None), (5, Some(&[][..]))], []);
    assert!(tick.iter().all(|(_, a)| a.len() == FRAME_SAMPLES));
}

#[test]
fn full_scale_converts_to_full_scale() {
    assert_eq!(to_f32(&[i16::MIN])[0], -1.0);
    assert!(to_f32(&[i16::MAX])[0] < 1.0);
}

#[test]
fn the_first_tick_is_announced_only_once() {
    let mut diag = TickDiag::new(Instant::now());
    assert!(diag.announce_first_tick());
    assert!(!diag.announce_first_tick());
    let _ = diag.report(Instant::now() + DIAG_INTERVAL, DIAG_INTERVAL);
    assert!(!diag.announce_first_tick());
}

#[test]
fn diag_names_the_ways_audio_can_be_missing() {
    let start = Instant::now();

    let mut diag = TickDiag::new(start);
    let report = diag.report(start + DIAG_INTERVAL, DIAG_INTERVAL).unwrap();
    assert!(report.contains("no voice ticks at all"), "{report}");

    let mut diag = TickDiag::new(start);
    diag.observe(&split_tick([], [1]), 0);
    let report = diag.report(start + DIAG_INTERVAL, DIAG_INTERVAL).unwrap();
    assert!(report.contains("carry no audio"), "{report}");

    let mut diag = TickDiag::new(start);
    diag.observe(&split_tick([(1, Some(&[30i16; 320][..]))], []), 1);
    let report = diag.report(start + DIAG_INTERVAL, DIAG_INTERVAL).unwrap();
    assert!(report.contains("very quietly"), "{report}");

    let mut diag = TickDiag::new(start);
    diag.observe(&split_tick([(1, Some(&[6000i16; 320][..]))], []), 1);
    let report = diag.report(start + DIAG_INTERVAL, DIAG_INTERVAL).unwrap();
    assert!(
        !report.contains("quietly") && !report.contains("no audio"),
        "{report}"
    );
}

#[test]
fn diag_stays_quiet_until_the_interval_passes() {
    let start = Instant::now();
    let mut diag = TickDiag::new(start);
    assert!(
        diag.report(start + Duration::from_secs(4), DIAG_INTERVAL)
            .is_none()
    );
    assert!(diag.report(start + DIAG_INTERVAL, DIAG_INTERVAL).is_some());
    assert_eq!(diag.ticks, 0, "counters reset after a report");
}
