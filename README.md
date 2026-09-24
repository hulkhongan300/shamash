# Shamash

A wake-word Discord music bot written in Rust.

Shamash joins a designated voice channel whenever a member enters it, listens
to what people say, and plays music when it hears a request:

> **"Shamash, play Dracula by Tame Impala"**

## How it works

```
Discord voice (Opus @ 48 kHz) ──► songbird receive
        │  VoiceTick → PCM (mono f32 48 kHz)
        ▼
Resampler (48 kHz → 16 kHz) ──► VAD buffer (flushes on silence)
        ▼
Whisper (local, via whisper-rs) → transcript
        ▼
Parser: wake word + "play <title> by <artist>" → search query
        ▼
yt-dlp search + download ──► songbird playback (ffmpeg decode)
```

- **Autonomous joins**: joins when a user enters the configured channel, leaves
  when it is empty.
- **Local speech-to-text**: Whisper runs on your machine, no audio leaves the
  host.
- **Minimal dependencies**: `serenity` + `songbird` + `whisper-rs` as crates;
  `yt-dlp` + `ffmpeg` as the only system tools for the music pipeline.

## Prerequisites

- Rust (edition 2024)
- System packages: `ffmpeg`, `yt-dlp`, `libopus-dev`, `pkg-config`, `cmake`
- A Discord application with a bot token (enable voice gateway intents)

## Setup

```sh
scripts/setup.sh   # installs nothing system-level; downloads the Whisper model
```

### Environment variables

| Variable          | Required | Description                                        |
| ----------------- | -------- | -------------------------------------------------- |
| `DISCORD_TOKEN`   | yes      | Bot token from the Discord developer portal        |
| `VOICE_CHANNEL_ID`| yes      | ID of the voice channel the bot watches and joins  |
| `WHISPER_MODEL`   | no       | Path to a Whisper model file (default `data/model.bin`) |
| `WAKE_WORDS`      | no       | Comma-separated wake words (default `shamash,bot`) |

## Run

```sh
cargo run --release
```

## Development

```sh
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test --all-targets
```

## License

MIT — see [LICENSE](LICENSE).