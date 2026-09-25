use crate::config::Config;
use crate::fetch;
use crate::parser::PlayRequest;
use crate::search;
use anyhow::Context;
use serenity::model::channel::ChannelType;
use serenity::model::id::{ChannelId, GuildId};
use serenity::prelude::Context as SerenityContext;
use songbird::Songbird;
use songbird::input::{File as FileInput, Input};
use std::sync::Arc;
use std::time::Duration;

/// Length of the acknowledgement tone.
const BEEP_DURATION: Duration = Duration::from_millis(150);
/// Tone frequency in Hz.
const BEEP_FREQUENCY: f32 = 880.0;
/// Songbird mixes at 48 kHz stereo, so the tone is encoded in that format
/// and needs no resampling.
const BEEP_SAMPLE_RATE: u32 = 48_000;
const BEEP_CHANNELS: u16 = 2;

/// Plays music for a request and reports the outcome to a text channel.
pub struct Player {
    pub manager: Arc<Songbird>,
}

impl Player {
    /// Plays a short tone so the speaker knows the wake word and command were
    /// both understood, without stopping whatever is currently playing.
    ///
    /// The queue is replaced by the music that follows, so the tone is given
    /// time to finish first.
    pub async fn acknowledge(&self, guild_id: GuildId) -> anyhow::Result<()> {
        let call = self
            .manager
            .get(guild_id)
            .context("bot is not in a voice channel")?;
        {
            let mut handler = call.lock().await;
            handler.play_input(Input::from(beep_wav()));
        }
        tokio::time::sleep(BEEP_DURATION + Duration::from_millis(50)).await;
        Ok(())
    }

    /// Searches YouTube for the most popular song matching the request and
    /// plays it, replacing whatever is currently playing. Confirmation is
    /// posted to the alert channel when one is available.
    pub async fn play(
        &self,
        ctx: &SerenityContext,
        config: &Config,
        guild_id: GuildId,
        request: &PlayRequest,
    ) -> anyhow::Result<()> {
        let call = self
            .manager
            .get(guild_id)
            .context("bot is not in a voice channel")?;
        let match_ = search::most_popular_song(&request.query, search::DEFAULT_CANDIDATES)
            .await
            .with_context(|| format!("no match for '{}'", request.query))?;
        let views = match match_.view_count {
            Some(views) => format!("{views} views"),
            None => "unknown views".to_string(),
        };
        let summary = format!(
            "Now playing \u{201c}{}\u{201d} by {} ({views})",
            match_.title(),
            match_.channel()
        );
        println!("[{guild_id}] {summary}");

        // Downloaded before the current track is stopped, so a failed fetch
        // leaves whatever is playing alone.
        let path = fetch::fetch(&match_.url, &fetch::cache_dir(guild_id.get()))
            .await
            .with_context(|| format!("could not fetch '{}'", match_.title()))?;
        println!("[{guild_id}] playing {}", path.display());

        let input = Input::from(FileInput::new(path));
        {
            let mut handler = call.lock().await;
            handler.stop();
            handler.play_input(input);
        }

        if let Some(channel) = resolve_alert_channel(ctx, config, guild_id)
            && let Err(e) = channel.say(&ctx.http, &summary).await
        {
            println!("failed to post play confirmation: {e}");
        }
        Ok(())
    }
}

/// Builds the acknowledgement tone as an in-memory 16-bit PCM WAV.
///
/// Songbird accepts a byte buffer directly as an input, so the tone needs no
/// file on disk. The attack and release are faded to keep it from clicking.
fn beep_wav() -> Vec<u8> {
    let frames = (BEEP_SAMPLE_RATE as f32 * BEEP_DURATION.as_secs_f32()).round() as usize;
    let bytes_per_frame = BEEP_CHANNELS as usize * 2;
    let data_len = frames * bytes_per_frame;
    let fade_frames = (BEEP_SAMPLE_RATE / 200) as f32;

    let mut wav = Vec::with_capacity(44 + data_len);
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&((36 + data_len) as u32).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes());
    wav.extend_from_slice(&BEEP_CHANNELS.to_le_bytes());
    wav.extend_from_slice(&BEEP_SAMPLE_RATE.to_le_bytes());
    let byte_rate = BEEP_SAMPLE_RATE as usize * bytes_per_frame;
    wav.extend_from_slice(&(byte_rate as u32).to_le_bytes());
    wav.extend_from_slice(&(bytes_per_frame as u16).to_le_bytes());
    wav.extend_from_slice(&16u16.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&(data_len as u32).to_le_bytes());

    let step = 2.0 * std::f32::consts::PI * BEEP_FREQUENCY / BEEP_SAMPLE_RATE as f32;
    for frame in 0..frames {
        let envelope = (frame as f32 / fade_frames)
            .min((frames - 1 - frame) as f32 / fade_frames)
            .clamp(0.0, 1.0);
        let tone = (step * frame as f32).sin() * i16::MAX as f32;
        let sample = (envelope * 0.3 * tone) as i16;
        for _ in 0..BEEP_CHANNELS {
            wav.extend_from_slice(&sample.to_le_bytes());
        }
    }
    wav
}

/// Picks the text channel used for feedback: the configured alert channel,
/// else the guild's system channel, else the first text channel.
fn resolve_alert_channel(
    ctx: &SerenityContext,
    config: &Config,
    guild_id: GuildId,
) -> Option<ChannelId> {
    if let Some(id) = config.alert_channel_id {
        return Some(ChannelId::new(id));
    }
    let guild = ctx.cache.guild(guild_id)?;
    guild.system_channel_id.or_else(|| {
        guild
            .channels
            .values()
            .find(|channel| channel.kind == ChannelType::Text)
            .map(|channel| channel.id)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reads a little-endian `u32` at `offset`.
    fn u32_at(wav: &[u8], offset: usize) -> u32 {
        wav[offset..offset + 4]
            .try_into()
            .map(u32::from_le_bytes)
            .unwrap()
    }

    /// Reads a little-endian `u16` at `offset`.
    fn u16_at(wav: &[u8], offset: usize) -> u16 {
        wav[offset..offset + 2]
            .try_into()
            .map(u16::from_le_bytes)
            .unwrap()
    }

    /// The tone has to survive the same probe songbird runs on it.
    ///
    /// Songbird depends on symphonia with `default-features = false` and exposes
    /// no feature to switch the format handlers back on, so its probe registry
    /// is empty unless symphonia is also a direct dependency of this crate. That
    /// build decodes nothing at all: every input fails with "no suitable format
    /// reader found", so the tone is silent and the music never starts.
    #[test]
    fn songbird_can_probe_and_decode_the_tone() {
        use symphonia::core::codecs::DecoderOptions;
        use symphonia::core::formats::FormatOptions;
        use symphonia::core::io::MediaSourceStream;
        use symphonia::core::meta::MetadataOptions;
        use symphonia::default::{get_codecs, get_probe};

        let wav = beep_wav();
        let source =
            MediaSourceStream::new(Box::new(std::io::Cursor::new(wav)), Default::default());
        let mut probed = get_probe()
            .format(
                &symphonia::core::probe::Hint::new(),
                source,
                &FormatOptions::default(),
                &MetadataOptions::default(),
            )
            .expect("songbird's probe must recognise the tone");
        let track = probed
            .format
            .default_track()
            .expect("the tone must expose a track");
        let mut decoder = get_codecs()
            .make(&track.codec_params, &DecoderOptions::default())
            .expect("the pcm decoder must be registered");

        let mut packets = 0;
        while let Ok(packet) = probed.format.next_packet() {
            decoder.decode(&packet).expect("pcm packet must decode");
            packets += 1;
        }
        assert!(packets > 0, "the tone must decode to audio samples");
    }

    #[test]
    fn beep_is_a_playable_wav_header() {
        let wav = beep_wav();
        assert_eq!(&wav[0..4], b"RIFF");
        assert_eq!(&wav[8..12], b"WAVE");
        assert_eq!(&wav[12..16], b"fmt ");
        assert_eq!(u32_at(&wav, 16), 16, "PCM fmt chunk size");
        assert_eq!(u16_at(&wav, 20), 1, "uncompressed PCM");
        assert_eq!(u16_at(&wav, 22), BEEP_CHANNELS);
        assert_eq!(u32_at(&wav, 24), BEEP_SAMPLE_RATE);
        assert_eq!(u16_at(&wav, 34), 16, "bits per sample");
        assert_eq!(&wav[36..40], b"data");
        let data_len = u32_at(&wav, 40) as usize;
        assert_eq!(wav.len(), 44 + data_len, "declared data length");
        assert_eq!(u32_at(&wav, 4) as usize, 36 + data_len, "RIFF length");
    }

    #[test]
    fn beep_holds_audible_tone_for_the_requested_duration() {
        let wav = beep_wav();
        let samples: Vec<i16> = wav[44..]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| i16::from_le_bytes(*c))
            .collect();
        let frames = samples.len() / BEEP_CHANNELS as usize;
        let expected = (BEEP_SAMPLE_RATE as f32 * BEEP_DURATION.as_secs_f32()) as usize;
        assert!(
            (frames as i32 - expected as i32).abs() <= 1,
            "{frames} frames"
        );

        let peak = samples.iter().map(|s| s.unsigned_abs()).max().unwrap();
        assert!(peak > i16::MAX as u16 / 5, "tone is too quiet: peak {peak}");

        let left = samples.iter().step_by(2);
        let right = samples.iter().skip(1).step_by(2);
        assert!(
            left.eq(right),
            "both channels carry the same tone, so it plays centred"
        );
    }

    #[test]
    fn beep_fades_in_and_out_instead_of_clicking() {
        let wav = beep_wav();
        let samples: Vec<i16> = wav[44..]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| i16::from_le_bytes(*c))
            .collect();
        let peak = samples.iter().map(|s| s.unsigned_abs()).max().unwrap();
        let first = samples[0].unsigned_abs();
        let last = samples[samples.len() - 2].unsigned_abs();
        assert!(first < peak / 4, "starts at {first}, peak {peak}");
        assert!(last < peak / 4, "ends at {last}, peak {peak}");
    }
}
