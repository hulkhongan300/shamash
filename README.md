# Shamash

A wake-word Discord music bot written in Rust.

Shamash sits in one voice channel, listens for its name, and plays the song
that is asked for:

> **"bot, play Dracula by Tame Impala"**

Speech recognition runs on your own machine through
[transcribe.cpp](https://github.com/handy-computer/transcribe.cpp), so no audio
leaves the host.

**Contents:** [Features](#features) · [Quick start](#quick-start) ·
[Using it](#using-it) · [Configuration](#configuration) ·
[How it works](#how-it-works) · [Troubleshooting](#troubleshooting) ·
[Development](#development) · [Acknowledgements](#acknowledgements)

## Features

- **Hands-free requests.** Say the wake word and a song, in one breath or in
  two. The music ducks to 20% while you speak, so it does not bleed into the
  request.
- **Always on, and cheap.** Only a voice detector runs continuously, at a few
  microseconds per frame. The small wake model runs only while someone is
  talking, and the large model only on an utterance that had the wake word in
  it.
- **Finds the right upload.** A request is resolved against YouTube Music's
  song catalog, which supplies the real title, artist, running time and the
  video the song is on. So the artist's own upload beats a remix, a lyric video
  or someone else's re-upload. If it is not sure, it asks instead of guessing.
- **`/play` with live suggestions.** Start typing and matching songs from
  YouTube Music appear. Pick one and that exact song is queued.
- **`/lock` and `/unlock`.** Turn listening off without removing the bot.
- **Autonomous.** Joins when someone enters the configured channel, including
  at startup, and leaves when it is empty.
- **A queue and a dashboard.** A now-playing message in the text channel has
  Pause, Skip, Queue, Repeat and Stop buttons. It is edited in place, so the
  channel does not fill up.
- **Fast playback.** Songs stream from YouTube while they are cached, and a
  song asked for again plays from the cache.

## Quick start

### 1. Create the Discord bot

1. In the [Discord developer portal](https://discord.com/developers/applications),
   create an application and add a bot. Copy its token.
2. Invite it with the `bot` and `applications.commands` scopes, and the
   **Connect**, **Speak**, **View Channel** and **Send Messages** permissions.
   No privileged intents are needed: the bot uses only `GUILDS` and
   `GUILD_VOICE_STATES`.
3. In Discord, turn on Developer Mode. Then right-click the voice channel the
   bot should listen in, and the text channel it should write to, and choose
   **Copy Channel ID** for each.

### 2. Configure it

```sh
cp .env.example .env
```

Fill in `DISCORD_TOKEN`, `VOICE_CHANNEL_ID` and `ALERT_CHANNEL_ID`. Everything
else is optional; see [Settings](#settings).

### 3. Run it

Either in Docker, which needs nothing else installed, or from source.

The two speech models, about 1 GB together, download on the first start and
are kept from then on. `MODEL_DOWNLOAD=off` turns that off, for a machine that
must not reach out; then fetch them yourself (see
[The two models](#the-two-models)).

The slash commands are registered with each server when the bot starts. If
`/play` suggestions do not show up straight after an update, reload Discord.

#### With Docker (recommended)

Needs Docker with Compose v2.24 or newer.

| GPU | Command |
| --- | --- |
| AMD or Intel | `docker compose up -d` |
| NVIDIA | `docker compose -f compose.yaml -f compose.nvidia.yaml up -d` |
| None | Remove the `devices:` lines from `compose.yaml`, then `docker compose up -d` |

Then `docker compose logs -f` to watch it start. The first start spends a
minute or two downloading the models. Look for `backend=Vulkan0` in the log:
that is the GPU being used. Without a GPU a request takes ~17 s instead of
~1 s; set the English-only `REQUEST_MODEL` in `.env` for ~0.1 s on a CPU.

- **AMD and Intel** work through `/dev/dri` and Mesa's drivers in the image.
- **NVIDIA** needs the
  [NVIDIA Container Toolkit](https://docs.nvidia.com/datacenter/cloud-native/container-toolkit/latest/install-guide.html)
  on the host.
- **Windows and macOS:** Docker Desktop cannot pass a GPU to Vulkan there, so
  the bot runs on the CPU. Run it from source for GPU speed.

Models, the song cache and the bot's data live in the `shamash-data` volume,
so updates keep them. yt-dlp updates itself at every start, because YouTube
breaks old versions within weeks.

```sh
docker compose pull && docker compose up -d   # update to the published image
docker compose build && docker compose up -d  # or rebuild from this checkout
```

To have a machine update itself whenever `main` changes, see
[deploy/README.md](deploy/README.md).

#### From source

- Rust (edition 2024)
- `ffmpeg`, `yt-dlp`, `libopus-dev` (or `opus`), `pkg-config`, `cmake`, and a
  C/C++ toolchain. `transcribe-cpp` builds its engine from source.
- For GPU inference, which is on by default: the Vulkan SDK headers and
  `glslc`. On Arch:

  ```sh
  sudo pacman -S --needed vulkan-headers spirv-headers shaderc
  ```

  Without them the build fails on a missing `SPIRV-Headers`. To build for the
  CPU only instead, drop the `vulkan` feature of `transcribe-cpp` from
  `Cargo.toml`.

Then, from the folder with the `.env`:

```sh
cargo run --release
```

## Using it

### Saying it

Any of these work:

- **"Bot, play Dracula by Tame Impala"**, in one breath. The request model is
  given the whole sentence, so nothing said along with the wake word is lost.
- **"Bot, Dracula by Tame Impala"**. "Play" is optional when the wake word
  starts the sentence. Mid-sentence ("I told the bot to play…") it is still
  needed, so mentioning the bot in conversation does not start a search.
- **"Bot."**, then wait for the second tone and say **"Dracula by Tame
  Impala"**. A wake word on its own arms that speaker for 8 seconds, and their
  next utterance is taken as the request.

The bot answers with tones:

| Tone | Meaning |
| --- | --- |
| 880 Hz | Wake word heard. The music drops to 20% |
| 988 Hz | Only the wake word was heard. Say the request now |
| 440 Hz | It could not find or play the song. A message in the text channel says what it heard |

The wake words are `bot` and `shamash` by default. Set `WAKE_WORDS` to change
them.

### Slash commands

| Command | What it does |
| --- | --- |
| `/play <song>` | Suggests songs as you type and queues the one you pick. A pasted YouTube or YouTube Music link plays that video |
| `/lock` | Stops listening. The bot stays in the channel and keeps playing |
| `/unlock` | Starts listening again |
| `/stop` | Stops playback, clears the queue and leaves the voice channel |

Every command needs you to be in the bot's voice channel.

### `/play`

Start typing after `/play` and the matching songs appear over the message box:

```
Tame Impala — Dracula (3:26)
Tame Impala — Dracula (feat. JENNIE) (3:30)
Iuly Neamtu — Dracula (Frappula) (2:17)
```

- **Pick a suggestion** and that exact song is queued. Each suggestion carries
  the video id YouTube Music lists for the song, so there is nothing left to
  search for.
- **Paste a YouTube link** and that video is queued.
- **Press Enter on your own text** without picking a suggestion, and the bot
  searches the catalog and replies with a numbered list, nine to a page:

  ```
  Which one?
  Searching the catalog for dracula.
  Page 1 of 3.
  1. Dracula        Tame Impala • 3:26
  2. Borderline     Tame Impala • 3:42
  ...
  [1][2][3][4][5]
  [6][7][8][9]
  [Next page][None of these]
  ```

  Every page is a window onto one search, so turning the page never searches
  again or reshuffles the list. That chooser is private to whoever typed the
  command.

If YouTube Music is slow or down, the suggestion box stays empty and pressing
Enter still works.

`/play` answers at once with "thinking…" and fills the reply in when the search
is done. Discord drops a command that is not answered within three seconds, and
a search can take most of that.

A chooser, from `/play` or from speech, is taken down after 30 seconds without
an answer, and a newer one replaces it.

### `/lock` and `/unlock`

`/lock` makes the bot drop everything it hears without running a model on it.
Playback, the dashboard and `/play` keep working. `/unlock` turns listening
back on. The reply is public, so the channel can see whether it is being
listened to. The lock is held in memory, so a restart starts listening again.

### The dashboard

The text channel has one now-playing message with the song's thumbnail and
these buttons:

| Button | Effect |
| --- | --- |
| Pause / Resume | Pauses or carries on with the current song |
| Skip | Starts the next song in the queue |
| Queue | Shows what is waiting, soonest first, with a **Back** button |
| Repeat | Replays the current song when it ends with nothing queued after it |
| Stop | Stops playback and clears the queue |

A song asked for while another is playing waits in the queue, which holds up to
20 songs. Repeat is repeat *one*: it never stops the next queued song from
playing.

Only members currently in the bot's voice channel can press these buttons.
Everyone else gets a private "join the voice channel first" and nothing happens.

### When the bot is not sure

When the bot is not confident which song or upload was meant, it plays nothing.
Instead it posts the closest results in the text channel as the same paged
chooser `/play` uses. A spoken request's chooser belongs to anyone in the voice
channel, because the bot hears a voice and cannot tell who spoke.

If no catalog knows the song, the YouTube results are offered as *unverified*
choices, and nothing plays until someone presses one. See
[Choosing the upload](#choosing-the-upload) for why.

## Configuration

Settings come from the environment, or from the `.env` file next to the
binary. Real environment variables win over the file. A malformed value stops
the bot at startup instead of silently falling back to a default.

### Settings

**Discord**

| Variable | Default | Description |
| --- | --- | --- |
| `DISCORD_TOKEN` | required | Bot token from the developer portal |
| `VOICE_CHANNEL_ID` | required | The voice channel the bot watches and joins |
| `ALERT_CHANNEL_ID` | system channel | Text channel for the dashboard and every other message. Without it, the server's system channel is used, then the first text channel |

**Listening**

| Variable | Default | Description |
| --- | --- | --- |
| `WAKE_WORDS` | `bot,shamash` | Comma-separated wake words |
| `WAKE_MODEL` | Parakeet TDT+CTC 110M | The small, always-loaded model. A `.gguf` path or a Hugging Face model id |
| `REQUEST_MODEL` | Whisper large-v3-turbo | The large model, loaded on the first wake word. A `.gguf` path or a Hugging Face model id. `PARAKEET_MODEL` is the old name and still works |
| `ASR_PROMPT` | an example request | Example text for models that take a prompt (Whisper does). `off` disables it |
| `ASR_LANGUAGE` | detected | One ISO-639-1 code, such as `en` or `pt` |
| `ASR_ENGINE` | `direct` | `direct` runs transcribe.cpp in-process. `handy` shells out to the Handy app, for comparing engines only |
| `VOICE_DIAG` | off | Set to `1` to log how loud each speaker is |
| `MODEL_DOWNLOAD` | on | Download a missing model at startup. `off` makes a missing model an error instead |
| `YTDLP_UPDATE` | on | Docker only: update yt-dlp at every start. `off` keeps the image's copy |

**Finding songs**

| Variable | Default | Description |
| --- | --- | --- |
| `RESOLVE_BUDGET_MS` | `2500` | How long the catalog lookup may take in total |
| `AMBIGUOUS_POLICY` | `ask` | When several artists recorded the title asked for: `ask` offers the choices, `auto` plays the most-played one, `popular` plays one only if it is far more popular |
| `SEARCH_CANDIDATES` | `8` | How many YouTube results a request is scored against, 1 to 50 |
| `MATCH_THRESHOLD` | `90` | The lowest score an upload may have and still play without asking |
| `UNVERIFIED_CHOOSER` | `true` | When no catalog knows the song, offer the YouTube results as a choice. With `false`, the bot only plays the miss tone |
| `LLM_ENDPOINT` | unset | An Ollama-compatible endpoint that rewrites misheard transcripts. See [The rewrite stage](#the-rewrite-stage) |
| `LLM_MODEL` | `llama3.2` | The model to use at that endpoint |

### The two models

The models are loaded differently, which is the whole reason there are two:

| | `WAKE_MODEL` | `REQUEST_MODEL` |
| --- | --- | --- |
| Loaded | at startup, always resident | on the first wake word, then resident |
| Runs | every 250 ms while someone is talking | once per utterance with the wake word in it |
| Job | hear one short word | turn the whole utterance into text |
| Default | `parakeet-tdt_ctc-110m` | `whisper-large-v3-turbo` |

Either setting takes a path to any `.gguf` that transcribe.cpp supports, or a
Hugging Face id resolved against the local cache. A model id that is not in the
cache yet is downloaded on startup. To fetch the defaults yourself instead:

```sh
huggingface-cli download handy-computer/parakeet-tdt_ctc-110m-gguf parakeet-tdt_ctc-110m-Q8_0.gguf
huggingface-cli download handy-computer/whisper-large-v3-turbo-gguf whisper-large-v3-turbo-Q8_0.gguf
```

**GPU.** Inference runs on the GPU through ggml's Vulkan backend when it is
available, and on the CPU otherwise. The startup log names the device each
model bound to (`backend=Vulkan0 device=Vulkan0`), so a fall back to the CPU
is visible. Whisper large-v3-turbo takes about 1 s per short request on a GPU
and about 17 s on a CPU.

**The prompt.** Whisper is given an example request as a prompt (`ASR_PROMPT`,
by default "Bot, play Dracula by Tame Impala. Play Bohemian Rhapsody by
Queen."). It continues in the style of its prompt, so names come back spelled
as names. On five synthesised requests for artists not in the prompt, it got
all five right. With `ASR_PROMPT=off` it wrote "Taman Pala" for Tame Impala.
Models that cannot take a prompt ignore it.

**English only and no GPU?** Parakeet v2 takes about 0.1 s per request:

```sh
REQUEST_MODEL=handy-computer/parakeet-tdt-0.6b-v2-gguf/parakeet-tdt-0.6b-v2-Q8_0.gguf
```

It cannot be prompted, though, and in the same comparison it spelled only two
of the five artists right.

**Language.** `ASR_LANGUAGE` takes one ISO-639-1 code: `en`, `pt` or `es`. It
does not take `pt-PT`, `pt_PT` or `Portuguese`, and the bot will not start with
a code its model does not know. Leave it unset if anyone speaks more than one
language. Whisper then detects among 100 languages, while pinning it to `en`
makes it render other languages as English words, and a misheard title is a
wrong song. Some models (Canary, Nemotron) produce no text without a hint, so
set one for those.

### Choosing wake words

Wake words longer than four letters tolerate one misheard character, so
"shamash" also answers to "shemash" and "shammash". Shorter ones must be heard
exactly, apart from one swapped vowel ("bot" heard as "but", which the small
model often does). Otherwise ordinary words like "not" or "boy" would wake it.

Avoid common words. "Play" as a wake word would make "let's play a game" a song
request.

### The rewrite stage

Speech-to-text writes a name as whatever letters it hears, such as "hoolig bts"
for "Hooligan, BTS", and a catalog can only match what it is given. A small
instruction model is good at guessing what was meant. Set `LLM_ENDPOINT` to try
it:

```sh
LLM_ENDPOINT=http://localhost:11434
LLM_MODEL=llama3.2
```

It is off by default, because the catalog already corrects most mishearing for
free. It is one HTTP request per command, limited to three seconds. Whatever
goes wrong, whether the endpoint is down, the model is missing or the reply is
not a song, the bot falls back to the words it heard. The rewrite is tried as a
second reading alongside the transcript, never instead of it.

### Tuning

These constants in the code decide how listening behaves:

| Constant | Default | Raise it to |
| --- | --- | --- |
| `wake::WAKE_WINDOW` | 2 s | hear a slower wake word, at the cost of more audio per check |
| `wake::WAKE_HOP` | 250 ms | use less GPU on a busy channel, at the cost of a later first tone |
| `wake::ARM_TTL` | 8 s | give people longer to say the request after a bare wake word |
| `segment::HANGOVER_FRAMES` | 640 ms | survive longer pauses inside a request, at the cost of a slower answer |
| `player::DUCK_VOLUME` | 20% | lower the music less while someone talks to the bot |

Measure a change with the [offline harness](#testing-without-discord).

## How it works

```
Discord voice (Opus @ 48 kHz) ──► songbird receive, decoded to 16 kHz mono
        │  one stream per speaker, never mixed
        ▼
┌─ per speaker, always on ─────────────────────────────────┐
│  VOICE DETECTOR (earshot): where speech starts and stops │
│  keeps every frame between, plus 300 ms of lead-in       │
│                                                          │
│  WAKE MODEL (small, resident): last 2 s of the utterance │
│  checked every 250 ms while the speaker is talking       │
└──────────────────────────────────────────────────────────┘
        │ wake word heard (mid-sentence is fine)
        ▼
      tone 1 (880 Hz) and the music drops to 20%
        │
        │ the speaker finishes
        ▼
┌─ on demand ──────────────────────────────────────────────┐
│  REQUEST MODEL: the whole utterance, wake word included  │
│        ▼                                                 │
│  LLM rewrite (optional): misheard names → likely names   │
└──────────────────────────────────────────────────────────┘
        │
        ├── wake word on its own ──► tone 2 (988 Hz); the speaker's
        │                            next utterance is the request
        ▼
Parser: "bot, [play] <title> by <artist>" → request
        ▼
YouTube Music catalog: the song's real artist, title, length and video
        ▼
the song's own video, or YouTube uploads scored against that length
        │
        ├──► not confident: a chooser in the text channel
        ▼
stream (cached alongside) ──► songbird ──► next in the queue when it ends
```

### Hearing

Each speaker's audio is cut into whole utterances by a small neural voice
detector ([earshot](https://crates.io/crates/earshot)). Every sample between
the start and end of speech is kept, including the quiet stretches and 300 ms
from before the onset, so soft consonants and the first syllable reach the
model intact. A loudness gate would drop exactly those, and quiet speakers
entirely.

While someone talks, the small wake model checks the last two seconds every
250 ms. Once it hears the wake word, the whole utterance goes to the large
model when the speaker stops, so the request is transcribed in one piece, wake
word included.

### Resolving the request

Speech-to-text mishears names, and a YouTube search on misheard words returns
whatever matched those letters. A music catalog, though, is a list of real
songs. Before anything is searched for, the request is looked up in YouTube
Music's song search, which returns the artist's own spelling of the title, the
track's running time, its play count, and the video the song is on. A match
that does not agree closely enough with what was heard is discarded in favour
of the transcript.

The search goes out as several queries at once: the free text, the artist's
name when it was heard right and the title was not, and the title alone when no
artist was named. The lookup has a 2.5 s budget (`RESOLVE_BUDGET_MS`). A slow
or failing catalog never blocks a request: the bot carries on with the words
it heard.

"Dracula (feat. JENNIE)" and "Dracula" are different recordings with the same
name. When both are listed, the plain title is taken as the song, so a request
for Dracula does not play the feature.

" by " is part of more song titles than you would think (*Stand By Me*, *By The
Way*), so the bot keeps every way of splitting the request and tries each in
turn. "stand by me by ben e king" is first tried as *Ben E. King — Stand By Me*.

Some titles are not distinctive. "Look at Me" has been recorded by Naughty Boy,
Tems, The Neighbourhood and others, and a request that names no artist gives
the catalog nothing to choose by. By default the bot offers the choices instead of aiming the search at an
artist nobody asked for. `AMBIGUOUS_POLICY=auto` plays the most-played one
instead.

### Choosing the upload

Ranking YouTube results by views picks the official video when the album track
was meant, and a remix when the original was. A search for *Tame Impala
Dracula*:

| result | length | views |
| --- | --- | --- |
| Official Video | 234 s | 75.5 M |
| JENNIE Remix (Lyrics) | 211 s | 50.8 M |
| **Dracula**, artist's channel | **206 s** | 2.8 M |

The catalog says the song runs 206 s, so the top two are not that song. When
the catalog knows the song's own video, that video is played directly. A video
search also tends to miss it: for *Tame Impala Dracula*, the artist's own audio
upload was ninth, behind a stranger's re-upload of the same audio.

Otherwise YouTube results are scored mainly on running time, then channel,
title and type of media. Views count only a little, to break ties.

Below `MATCH_THRESHOLD` the bot asks instead of playing. When the catalog did
not know the song, nothing plays unless someone presses a choice: without a verified
running time, the scorer cannot tell the studio recording from a remix of it.

### Playback and caching

A song usually plays before it is cached. One yt-dlp call lists the video's
formats, and the AAC stream is played straight from YouTube while it is written
to the cache in the background. The first audio then arrives after a few
kilobytes instead of the whole file. On a ten-song cold benchmark the median
went from about 2.2 s to about 1.8 s.

The stream URL is signed and expires. If YouTube answers 403, or nothing
arrives within 2.5 s, the song is downloaded and played from the cache instead.

Each song is cached under its video id in `$XDG_CACHE_HOME/shamash/` (by
default `~/.cache/shamash/`). The newest few downloads are kept, so a repeated
request is instant and the cache does not grow without bound.

## Troubleshooting

### Logs

By default the bot logs what it heard, what it did about it, and which models
loaded:

```
INFO shamash::direct: loaded ASR model model=... backend=Vulkan0 device=Vulkan0
INFO shamash::bot: Shamash starting up request_model=... wake_model=... wake_words=bot, shamash
INFO shamash::pipeline: request heard guild_id=... user=... transcript="Bot, play Dracula by Tame Impala."
INFO shamash::pipeline: playing a request guild_id=... request="dracula by tame impala"
```

Set `RUST_LOG=shamash=debug` to also see what the wake model heard on each
check, the catalog match, and the scores of the winner and the runner-up:

```
wake model heard: "shamash play dracula"
resolved against the catalog provider=ytmusic catalog=Dracula — Tame Impala
  duration=Some(206.0) confidence=0.94 query=Tame Impala Dracula
chose a video heard=drakula tame impala picked=Dracula channel=Tame Impala
  chosen=total 124.0 (duration +100.0, channel +30.0, relevance +18.0,
  media +0.0, views +6.0) runner_up=Dracula (Lyrics) (101.4)
  threshold=90.0 confident=true
```

Serenity's and songbird's routine logging is silenced, but their warnings
still come through. One warning is always dropped:
`RTCP decryption failed: Crypto(Error)`. Discord sends reports for streams the
bot has no key for, so it fires constantly on a working connection. The
similar `RTP decryption failed` and `Decode error for SSRC` are kept, because
they mean the bot has stopped hearing.

### Common problems

| Symptom | Likely cause |
| --- | --- |
| No tone at all | Listening is locked (`/unlock`), or the wake word is not recognised. Check the debug log for what the wake model heard, and adjust `WAKE_WORDS` |
| A tone, then nothing plays | Look in the text channel: there is either a chooser or a message saying what was heard |
| The wrong song plays | Name the artist. Check `RUST_LOG=shamash=debug` for the scores |
| The first request after startup is slow | The request model loads on the first wake word. Later requests are fast |
| Requests take many seconds | The model is running on the CPU. Check the `backend=` in the startup log |
| `/play` suggestions do not appear | Restart the bot so it re-registers the command, then reload Discord |
| The build fails on `SPIRV-Headers` | Install the Vulkan packages, or drop the `vulkan` feature |

### Testing without Discord

Replay a recording through the real listening pipeline:

```sh
cargo run --release --example transcribe -- recording.wav
```

It feeds the WAV in 20 ms ticks the way Discord delivers audio, through the
real voice detector and wake gate. Then it transcribes every utterance the gate
hands over with the request model, and applies the bot's grammar:

```
  [  5.42s] wake model heard: "Bart play."
  [  5.92s] wake model heard: "But play draccupy."
>>> [  5.92s] wake word heard (tone 1)
larger model ready in 2.86s
>>> utterance of 2.58 s transcribed in 1.17s: "Bot, play Dracula by Tame Impala."
play: dracula by tame impala
>>> [  9.26s] wake word heard (tone 1)
>>> utterance of 0.87 s transcribed in 1.09s: "But,"
wake word on its own: waiting for the request (tone 2)
>>> utterance of 2.21 s transcribed in 1.08s: "Bohemian Rhapsody by Queen."
play: bohemian rhapsody by queen
```

It reads the same settings as the bot, so re-run it after changing a model,
the wake words, the language or the prompt. It also prints the loudest 20 ms
frame. Set `VOICE_DIAG=1` on the bot for the same report live.

If no utterance is handed over, the harness says how many wake checks it made.
Zero means the voice detector heard no speech in the file. More than zero means
speech was heard but the wake word was not recognised, which is a `WAKE_WORDS`
problem.

## Development

```sh
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test --all-targets
```

## Acknowledgements

Shamash stands on the work of the **handy-computer** group, and owes them a
genuine thank you.

The speech engine is
[transcribe.cpp](https://github.com/handy-computer/transcribe.cpp), their Rust
bindings to a C/C++ speech-to-text library built on ggml. It is what Shamash
listens with. Before it, this bot transcribed through the
[Handy](https://github.com/cjpais/Handy) app as a subprocess, which cost a Tauri
startup and a full model reload on *every* utterance. Linking transcribe.cpp
directly and holding the model resident was the single biggest change in how
this bot performs, from roughly 350 ms per command down to inference alone.
Turning on ggml's Vulkan backend afterwards took a Whisper decode from ~17 s to
~1 s on this machine.

The library covers 16+ model families behind one API, and it keeps the model
resident so a caller can use it the way the caller actually wants to. It has
been a great building block, and the kind of dependency that makes a project
like this possible at all.

The model catalogue and the GGUF conversions come from the same group too,
which is why both default models come straight from their Hugging Face
repositories.

If you are the maintainers: thank you, and please keep going.

## License

MIT — see [LICENSE](LICENSE).
