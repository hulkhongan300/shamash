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
- **Picks the popular song**: a request searches YouTube and plays the
  most-viewed result that looks like a real song — clips, covers, remixes,
  lyric videos, and long mixes are skipped.
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
```

Then create a gitignored `.env` next to the binary with your own values:

```sh
cat > .env <<'EOF'
DISCORD_TOKEN=your-bot-token
VOICE_CHANNEL_ID=your-voice-channel-id
WAKE_WORDS=bot
WHISPER_MODEL=data/model.bin
EOF
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
| `WAKE_WORDS`      | no       | Comma-separated wake words (default `bot`) |
| `ALERT_CHANNEL_ID`| no       | Text channel for play confirmations (defaults to the server's system channel, then to the first text channel) |

Secrets live in a gitignored `.env` file next to the binary, loaded through
[`dotenvy`](https://crates.io/crates/dotenvy). Real environment variables take
precedence over the file.

The default wake word is "bot". Wake words longer than four letters tolerate one
misheard character, so a longer custom word like "shamash" would still answer to
"shemash" and "shammash". Shorter ones such as "bot" must be heard exactly,
otherwise ordinary speech ("not", "boy") would trigger the bot.

## Run

```sh
cargo run --release
```

## Testing without Discord

Record a WAV of yourself saying *"Bot, play Dracula by Tame Impala"* and
run the transcriber end-to-end (resample → VAD → Whisper → parser):

```sh
cargo run --release --example transcribe -- recording.wav
```

It prints what the bot hears and which play request it would make, plus a level
report: the loudest 20 ms frame against the voice-activity gate. If that frame
sits below the gate, the bot cannot hear you at all, so the report is the first
thing to check when it stays silent. `VAD_RMS_THRESHOLD` in `src/listener.rs`
sets the gate; it defaults to 0.02, which is -34 dBFS against a typical
conversational voice at -20 dBFS.

## Development

```sh
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test --all-targets
```

## License

MIT — see [LICENSE](LICENSE).