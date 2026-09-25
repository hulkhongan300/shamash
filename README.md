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
- **Minimal dependencies**: `serenity` + `songbird` + `whisper-rs` as crates
  (+ `dotenvy` for `.env` loading); `yt-dlp` + `ffmpeg` as the only system
  tools for the music pipeline.

## Prerequisites

- Rust (edition 2024)
- System packages: `ffmpeg`, `yt-dlp`, `libopus-dev`, `pkg-config`, `cmake`
- A Discord application with a bot token (enable voice gateway intents)

## Setup

```sh
scripts/setup.sh        # downloads a Whisper model to data/model.bin
cp .env.example .env    # then fill in DISCORD_TOKEN and VOICE_CHANNEL_ID
```

The model download takes a minute or so. `scripts/setup.sh` defaults to
`base.en-q5_1` (great low-power balance); pass any of `tiny.en tiny base.en
base small.en small` (optionally `-q5_1`/`-q8_0` variants) to pick another.

### Environment variables

| Variable          | Required | Description                                        |
| ----------------- | -------- | -------------------------------------------------- |
| `DISCORD_TOKEN`   | yes      | Bot token from the Discord developer portal        |
| `VOICE_CHANNEL_ID`| yes      | ID of the voice channel the bot watches and joins  |
| `WHISPER_MODEL`   | no       | Path to a Whisper model file (default `data/model.bin`) |
| `WAKE_WORDS`      | no       | Comma-separated wake words (default `shamash,bot`) |
| `ALERT_CHANNEL_ID`| no       | Text channel for play confirmations (defaults to the server's system channel, then to the first text channel) |

Secrets live in a gitignored `.env` file next to the binary, loaded through
[`dotenvy`](https://crates.io/crates/dotenvy). Real environment variables take
precedence over the file.

## Run

```sh
cargo run --release
```

## Testing without Discord

Record a WAV of yourself saying *"Shamash, play Dracula by Tame Impala"* and
run the transcriber end-to-end (resample → VAD → Whisper → parser):

```sh
cargo run --release --example transcribe -- recording.wav
```

It prints what the bot hears and which play request it would make.

## Development

```sh
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test --all-targets
```

## License

MIT — see [LICENSE](LICENSE).