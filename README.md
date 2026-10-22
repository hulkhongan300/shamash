# Shamash

A wake-word Discord music bot written in Rust. It sits in a voice channel,
listens for its name, and plays the song it is asked for:

> **"bot, play Dracula by Tame Impala"**

Speech is transcribed on your machine with
[transcribe.cpp](https://github.com/handy-computer/transcribe.cpp) — no audio
leaves the host.

## What it does

- **Joins by itself**: enters the configured voice channel when someone is in
  it, leaves when it empties.
- **Listens cheaply**: a small model is always on but only runs on speech; a
  larger one loads on the wake word and transcribes just the request.
- **Plays the right version**: requests are checked against YouTube Music for
  the real artist, title and length, so the album track beats a remix with more
  views.
- **Queues and controls**: up to 20 waiting songs, a now-playing dashboard with
  buttons, and a `/play` chooser for when you would rather type.
- **Tells you what happened**: a tone when it hears you, another when it is
  ready, a lower one when it could not find the song.

## Prerequisites

- Rust (edition 2024)
- `ffmpeg`, `yt-dlp`, `libopus-dev`, `pkg-config`, `cmake`, and a C/C++ toolchain
- Vulkan headers, `glslc` and SPIR-V headers for GPU inference (on Arch:
  `sudo pacman -S --needed vulkan-headers spirv-headers shaderc`). Without them
  the build fails; drop the `vulkan` feature from `transcribe-cpp` in
  `Cargo.toml` to run on the CPU instead.
- A Discord application with a bot token and voice intents enabled

## Setup

Fetch the two models into the Hugging Face cache, where they are looked for by
default:

```sh
huggingface-cli download handy-computer/parakeet-tdt_ctc-110m-gguf parakeet-tdt_ctc-110m-Q8_0.gguf
huggingface-cli download handy-computer/whisper-large-v3-turbo-gguf whisper-large-v3-turbo-Q8_0.gguf
```

`WAKE_MODEL` and `PARAKEET_MODEL` also take a path to any `.gguf` on disk, or a
Hugging Face model id. The wake model is loaded at startup; if it cannot be
found the bot refuses to start.

Then write a gitignored `.env` in the working directory:

```sh
cat > .env <<'EOF'
DISCORD_TOKEN=your-bot-token
VOICE_CHANNEL_ID=your-voice-channel-id
ALERT_CHANNEL_ID=your-text-channel-id
EOF
```

With Developer Mode on in Discord, right-click a channel and choose "Copy
Channel ID". `ALERT_CHANNEL_ID` can be omitted; messages then go to the server's
system channel, or the first text channel it can post in.

## Run

```sh
cargo run --release
```

Slash commands are registered per guild as the bot joins, so `/play` is
available immediately.

## Using it

Say the wake word followed by a request. With the default wake words, all of
these work:

```
bot, play Dracula by Tame Impala
play Dracula by Tame Impala
```

A wake word alone (`bot, play`) makes the bot wait up to 8 seconds for the rest
of the request. `WAKE_WORDS` accepts one-word wake words, comma separated; words
longer than four letters tolerate a single misheard character.

When a request is not clear enough, the bot does not guess: it posts choices in
the alert channel and waits (30 seconds) for a press. With
`UNVERIFIED_CHOOSER=true` (the default) it also offers unverified uploads when
the catalog has never heard of the song. Nothing plays unasked.

### `/play`

```
/play dracula
```

Searches the catalog and posts the results as numbered buttons, nine per page,
with **Next page** and **None of these**. Choosing a suggestion from
autocomplete, or pasting a YouTube URL, plays it directly. A `/play` chooser
belongs to whoever typed it; a chooser raised by speech belongs to the voice
channel. You must be in the bot's voice channel to run it.

### Controls

The now-playing message in the alert channel is the shared control surface:

| Button        | Effect                                                    |
| ------------- | --------------------------------------------------------- |
| Pause/Resume  | Pauses the current song, and carries on from the same spot |
| Skip          | Drops the current song and starts the next one             |
| Queue         | Lists what is waiting, with a **Back** button              |
| Repeat        | Repeats the current song when nothing is queued behind it  |
| Stop          | Stops playback; the waiting queue is kept                  |

Only members in the bot's voice channel can press these. Everyone else gets a
private "join the voice channel first" refusal, because the dashboard is a
text-channel message anyone could otherwise press.

`/stop` is different: it stops playback and makes the bot leave the channel, so
it stops listening until someone comes back.

## How it works

```
voice (Opus 48 kHz) → mono 16 kHz PCM
        ↓
  wake model, always resident: 2 s rolling window, checked every 250 ms on speech
        ↓ wake word
  880 Hz tone → load the large model → 988 Hz tone
        ↓ capture up to 5 s, or 500 ms of silence
  request model → parser: "play <title> by <artist>"
        ↓
  YouTube Music catalog: real artist, title and length
        ↓
  ranked YouTube uploads → stream (cache in background) → songbird
        ↓ nothing convincing
  choices in the alert channel, or a 440 Hz miss tone and a message
```

Two models exist because they are paid for differently: the small one is cheap
enough to run continuously, and the large one is loaded only after a wake word,
then kept resident. Reloading it per command would cost far more than holding
it.

Requests are resolved against the catalog first, because uploader-written titles
are not a list of songs and view counts reward remixes and lyric videos. The
upload is then scored on title, artist, duration and channel; popularity only
breaks ties. A request the catalog cannot verify is never played unasked.

## Configuration

All settings are optional apart from the token and channel:

| Variable            | Default | Description                                                        |
| ------------------- | ------- | ------------------------------------------------------------------ |
| `DISCORD_TOKEN`     | —       | Bot token (required)                                               |
| `VOICE_CHANNEL_ID`  | —       | Voice channel to watch and join (required)                         |
| `ALERT_CHANNEL_ID`  | system channel | Text channel for the dashboard and every other message      |
| `WAKE_WORDS`        | `bot,play,ut,ot` | Comma-separated wake words                                |
| `WAKE_MODEL`        | Parakeet TDT+CTC 110M | Small always-on model: `.gguf` path or HF id          |
| `PARAKEET_MODEL`    | Whisper large-v3-turbo Q8_0 | Large on-demand model                          |
| `ASR_ENGINE`        | `direct` | `direct` (in-process transcribe.cpp) or `handy` (subprocess)      |
| `ASR_LANGUAGE`      | auto    | One ISO-639-1 code, e.g. `en`. `pt-PT` is not valid                |
| `MATCH_THRESHOLD`   | `90`    | Lowest upload score played without asking, 0–1000                  |
| `SEARCH_CANDIDATES` | `8`     | YouTube results scored per request, 1–50                          |
| `AMBIGUOUS_POLICY`  | `ask`   | `ask`, `auto` (play the best same-title match) or `popular`        |
| `UNVERIFIED_CHOOSER`| `true`  | Offer unverified uploads when nothing matched                     |
| `RESOLVE_BUDGET_MS` | `2500`  | Time budget for the catalog lookup                                |

Malformed numbers and booleans are startup errors rather than silent defaults.
Real environment variables take precedence over `.env`. `RUST_LOG=shamash=debug`
shows what the wake model heard, the catalog resolution and the scores.

### Tuning the wake word

The always-on gate is the part most likely to need adjusting on a real voice.
Three constants in `src/wake.rs`:

| Constant          | Default | Raise it to                                          |
| ----------------- | ------- | ---------------------------------------------------- |
| `WAKE_WINDOW`     | 2 s     | hear a slower wake word, at more transcription cost  |
| `WAKE_HOP`        | 250 ms  | use less CPU, at the cost of missing a quick shout    |
| `WAKE_TAIL_FRAMES`| 500 ms  | hear a wake word at the end of a sentence            |

Each check is one inference on the small model, so a busy channel costs about
four a second. The VAD gate (`VAD_RMS_THRESHOLD` in `src/listener.rs`, 0.02)
decides what counts as speech at all.

## Testing without Discord

Record yourself saying the wake word, pausing, then a request. The pause matters:
it is where the two tones fall.

```sh
cargo run --release --example transcribe -- recording.wav
```

This drives the real wake gate and prints every window the small model heard,
whether the wake word fired, and what the large model made of the request. It
also reports the loudest 20 ms frame against the VAD gate — the first thing to
check when the bot seems deaf.

`WAKE_MODEL`, `PARAKEET_MODEL` and `WAKE_WORDS` are read from the same settings
as the bot. For CPU-only timings there is `bench_direct`, and `bench_pipeline`
and `bench_stream` measure the request and streaming paths.

## Development

```sh
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test --all-targets
```

The test suite needs no Discord connection or models.

## Acknowledgements

The speech engine is [transcribe.cpp](https://github.com/handy-computer/transcribe.cpp)
from the **handy-computer** group, who also publish the GGUF conversions used by
the defaults. Linking it in-process instead of spawning the
[Handy](https://github.com/cjpais/Handy) app removed a full model reload from
every command, making it the biggest performance change in this project. Thank
you.

## License

MIT — see [LICENSE](LICENSE).
