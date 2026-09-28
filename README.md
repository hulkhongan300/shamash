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
Parakeet (local, transcribe.cpp linked in) → transcript
        ▼
Parser: wake word + "play <title> by <artist>" → search query
        ▼
yt-dlp search + download ──► per-song file ──► songbird playback
        │
        ├──► dashboard embed in the alert channel (title, thumbnail, buttons)
        ▼
songbird reports the song ended ──► next song in the queue starts
```

- **Autonomous joins**: joins when a user enters the configured channel, leaves
  when it is empty.
- **Picks the popular song**: a request searches YouTube and plays the
  most-viewed result that looks like a real song — clips, covers, remixes,
  lyric videos, and long mixes are skipped.
- **Queues what it is asked for**: ask for a second song while one is playing
  and it waits, starting on its own when the current one ends.
- **Dashboard controls**: the alert channel carries a now-playing embed with
  the song's thumbnail and Pause/Resume, Skip and Stop buttons. The message is
  edited in place rather than replaced, so the channel does not fill up.
- **Local speech-to-text**: transcription runs on your machine, no audio leaves
  the host.
- **Few dependencies**: `serenity` + `songbird` + `transcribe-cpp` as crates
  (+ `dotenvy` for `.env` loading); `yt-dlp` + `ffmpeg` for the music pipeline.

## Prerequisites

- Rust (edition 2024)
- System packages: `ffmpeg`, `yt-dlp`, `libopus-dev`, `pkg-config`, `cmake`,
  and a C/C++ toolchain (`transcribe-cpp` builds its engine from source)
- A Parakeet GGUF model. The default one comes from the
  [Handy](https://github.com/cjpais/Handy) app's Hugging Face cache, so
  installing Handy once is the easiest way to get it
- A Discord application with a bot token (enable voice gateway intents)

## Setup

Install the [Handy](https://github.com/cjpais/Handy) app once and pick a
Parakeet model in it, so the weights land in the Hugging Face cache that
`PARAKEET_MODEL` resolves against. Or skip it and point `PARAKEET_MODEL`
straight at any transcribe.cpp GGUF.

Then create a gitignored `.env` next to the binary with your own values:

```sh
cat > .env <<'EOF'
DISCORD_TOKEN=your-bot-token
VOICE_CHANNEL_ID=your-voice-channel-id
ALERT_CHANNEL_ID=your-text-channel-id
EOF
```

`VOICE_CHANNEL_ID` is the channel the bot listens in, `ALERT_CHANNEL_ID` the one
it writes to. With Developer Mode on in Discord, right-click either channel and
choose "Copy Channel ID" to fill them in.

Speech-to-text links [transcribe.cpp](https://github.com/cjpais/Handy) into
the bot and keeps the model loaded, so an utterance costs inference time only.
Measured here on a 2 s utterance with Parakeet TDT+CTC 110M: 90 ms in-process
versus 346 ms through `handy --transcribe-file`, which reloads the model and
starts a Tauri app every time. `cargo run --release --example bench_direct --
recording.wav` prints the same split.

`PARAKEET_MODEL` takes either a path to a `.gguf` or a Handy catalogue id, which
is resolved against the Hugging Face cache. Some models (Canary, Nemotron) need
a language hint to produce text at all; set `ASR_LANGUAGE=en` for those.

`ASR_ENGINE=handy` restores the old subprocess behaviour, which is slower but
useful for comparing engines on the same audio.

Whisper models work the same way, as GGUFs, through this engine. The old
`whisper-rs` engine and `scripts/setup.sh` are gone: it vendored its own copy
of `ggml` and could not be linked alongside transcribe.cpp, so one had to go.
`ASR_ENGINE=whisper` now fails with a message saying so.

### Environment variables

| Variable          | Required | Description                                        |
| ----------------- | -------- | -------------------------------------------------- |
| `DISCORD_TOKEN`   | yes      | Bot token from the Discord developer portal        |
| `VOICE_CHANNEL_ID`| yes      | ID of the voice channel the bot watches and joins  |
| `ASR_ENGINE`      | no       | `direct` (default, in-process transcribe.cpp) or `handy` (subprocess) |
| `PARAKEET_MODEL`  | no       | Path to a `.gguf`, or a Handy model id; the Handy cache's Parakeet TDT+CTC 110M by default |
| `ASR_LANGUAGE`    | no       | Language hint for models that need one, e.g. `en` for Canary or Nemotron |
| `WAKE_WORDS`      | no       | Comma-separated wake words (default `bot,play,ut,ot`) |
| `ALERT_CHANNEL_ID`| no       | Text channel for the now-playing dashboard and every other bot message (defaults to the server's system channel, then to the first text channel) |

Secrets live in a gitignored `.env` file next to the binary, loaded through
[`dotenvy`](https://crates.io/crates/dotenvy). Real environment variables take
precedence over the file.

Wake words longer than four letters tolerate one misheard character, so a
custom word like "shamash" would still answer to "shemash" and "shammash".
Shorter ones must be heard exactly, otherwise ordinary speech ("not", "boy")
would trigger the bot — which is why "ut" and "ot" are listed alongside "bot":
speech-to-text often drops the first consonant of a short word.

## Run

```sh
cargo run --release
```

### Controlling playback

The dashboard in the alert channel is the only control surface:

| Button   | Effect                                                     |
| -------- | ---------------------------------------------------------- |
| Pause    | Pauses the current song; the button becomes Resume         |
| Resume   | Carries on from where the song paused                        |
| Skip     | Drops the current song and starts the next one queued       |
| Stop     | Stops playback and clears the queue                         |

`/stop` is separate: it also leaves the voice channel, so the bot stops
transcribing until someone comes back.

Each song is downloaded to its own file, named after its video id, so a
request can never be served the audio of an earlier one. The newest few
downloads are kept and the rest pruned, which keeps a repeated request
instant and stops the cache growing without bound.

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