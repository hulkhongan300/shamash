# Shamash

A wake-word Discord music bot written in Rust.

Shamash joins a designated voice channel whenever anyone is in it — including
when it starts up and finds the channel already occupied — listens for its
name, and plays the song that is asked for:

> **"bot, play Dracula by Tame Impala"**

It hears with [transcribe.cpp](https://github.com/handy-computer/transcribe.cpp),
linked in-process from the [handy-computer](https://github.com/handy-computer)
group — see [Acknowledgements](#acknowledgements).

## How it works

The listening is staged, because a conversation in a voice channel should not
cost a large transcription, and a person who says the bot's name should not have
to wait for it to think before they know they were heard.

```
Discord voice (Opus @ 48 kHz) ──► songbird receive
        │  VoiceTick → PCM (mono f32 16 kHz)
        ▼
┌─ always on ──────────────────────────────────────────────┐
│  WAKE MODEL — small, loaded at startup, held resident     │
│  rolling 2 s window, checked every 500 ms while speech    │
└──────────────────────────────────────────────────────────┘
        │ wake word heard
        ▼
      tone 1  (880 Hz)  "I heard you"
        │
        │ load the request model (once; then resident)
        ▼
      tone 2  (988 Hz)  "ready, go ahead"
        │
        │ capture up to 5 s, or until 500 ms of silence
        ▼
┌─ on demand ──────────────────────────────────────────────┐
│  PARAKEET_MODEL — large, transcribes only what was said  │
│        ▼                                                   │
│  LLM rewrite (optional) — misheard names → likely names  │
└──────────────────────────────────────────────────────────┘
        │
        ▼
Parser: "play <title> by <artist>" → search query
        ▼
Catalog (YouTube Music): the song's real artist, title and length
        ▼
catalog video id, or yt-dlp search when it has none ──► stream, cached per song ──► songbird
        │
        ├──► if no result is convincing enough, three buttons to pick from
        ▼
songbird reports the song ended ──► next song in the queue starts
```

### The two tones

The first tone means the wake word was heard. The second means the larger model
is loaded and listening. They are a tone apart rather than a melody, so the pair
is recognisably one acknowledgement, but a person who starts their request
before the second finishes can still tell which stage answered them.

The larger model is loaded *after* a wake word rather than at startup, which is
what gives the second tone something true to report. It is then kept resident:
reloading a 700 MB model per command would cost far more than holding it, and
holding it is the whole reason the in-process engine is worth having.

- **Always-on and cheap**: only the small model runs continuously, and only on
  windows that contain speech. A quiet channel costs one RMS per frame.
- **Autonomous joins**: joins when a user enters the configured channel, leaves
  when it is empty.
- **Picks the popular song**: a request searches YouTube and plays the
  most-viewed result that looks like a real song — clips, covers, remixes,
  lyric videos, and long mixes are skipped.
- **Queues what it is asked for**: ask for a second song while one is playing
  and it waits, starting on its own when the current one ends.
- **Dashboard controls**: the alert channel carries a now-playing embed with
  the song's thumbnail and Pause/Resume, Skip, Queue, Repeat and Stop buttons.
  The message is edited in place rather than replaced, so the channel does not
  fill up.
- **Search it yourself**: `/play <song>` posts a paged list from the catalog and
  plays whichever one you press
- **Local speech-to-text**: transcription runs on your machine, no audio leaves
  the host.
- **Few dependencies**: `serenity` + `songbird` + `transcribe-cpp` as crates
  (+ `dotenvy` for `.env` loading); `yt-dlp` + `ffmpeg` for the music pipeline.

## Prerequisites

- Rust (edition 2024)
- System packages: `ffmpeg`, `yt-dlp`, `libopus-dev`, `pkg-config`, `cmake`,
  and a C/C++ toolchain (`transcribe-cpp` builds its engine from source)
- **Two** GGUF models: a small one for the wake word and a large one for
  requests. Both are plain files you can download yourself — see [Setup](#setup)
- A Discord application with a bot token (enable voice gateway intents)
- Optional, for the rewrite stage: [Ollama](https://ollama.com) with a small
  model pulled

## Setup

Fetch the two models into the Hugging Face cache, which is where `WAKE_MODEL`
and `PARAKEET_MODEL` look by default:

```sh
huggingface-cli download handy-computer/parakeet-tdt_ctc-110m-gguf parakeet-tdt_ctc-110m-Q8_0.gguf
huggingface-cli download handy-computer/whisper-large-v3-turbo-gguf whisper-large-v3-turbo-Q8_0.gguf
```

Either variable also takes a plain path to a `.gguf` anywhere on disk, so any
model transcribe.cpp supports will do.

You do not need the Handy app. The speech engine is
[transcribe.cpp](https://github.com/handy-computer/transcribe.cpp), linked into
this process and built from source by `cargo`; see
[Acknowledgements](#acknowledgements). `ASR_ENGINE=handy` still shells out to the
Handy binary, but only as an opt-in way to compare the two engines on the same
audio.

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

### The two models

They pay for being loaded differently, which is the whole reason there are two:

| | `WAKE_MODEL` | `PARAKEET_MODEL` |
| --- | --- | --- |
| Loaded | at startup, always resident | on the first wake word, then resident |
| Runs | on every window of speech in the channel | only on a request, after a wake word |
| Job | hear one short word | turn the request into text |
| Default | `parakeet-tdt_ctc-110m` | `whisper-large-v3-turbo` |

`ASR_ENGINE=handy` swaps the in-process engine back to the Handy subprocess,
which reloads a model per call. It is slower and kept for comparing engines on
the same audio; the staged design assumes the resident one.

Inference runs on the GPU through ggml's Vulkan backend when the build has it
compiled in and a device is registered, and on the CPU otherwise. The `vulkan`
feature of `transcribe-cpp` is on; building it needs the Vulkan SDK headers and
`glslc`, so on a fresh Arch box:

```sh
sudo pacman -S --needed vulkan-headers spirv-headers shaderc
```

The startup log names the device each model actually bound to
(`backend=Vulkan0 device=Vulkan0`), so a silent fallback to the CPU is visible
rather than inferred. Nothing to configure: the engine picks the best device it
has.
Without those packages the build fails on a missing `SPIRV-Headers`, so drop the
feature from `Cargo.toml` to go back to CPU inference.

Whisper models work the same way, as GGUFs, through this engine. The old
`whisper-rs` engine and `scripts/setup.sh` are gone: it vendored its own copy
of `ggml` and could not be linked alongside transcribe.cpp, so one had to go.
`ASR_ENGINE=whisper` now fails with a message saying so.

`PARAKEET_MODEL` takes either a path to a `.gguf` or a Hugging Face catalogue id,
which is resolved against the Hugging Face cache. Some models (Canary,
Nemotron) need a language hint to produce text at all; set `ASR_LANGUAGE=en` for
those. The Whisper default detects the language itself and does not need one.

`ASR_LANGUAGE` takes **one** ISO-639-1 code — `en`, `pt`, `es`. Not `pt-PT`,
not `pt_PT`, not `Portuguese`: those all fail, and they fail at the first
utterance rather than at startup, so the bot used to join the channel and then
refuse every transcription. That is checked at startup now, and the bot will not
start with a language its model does not know.

Whisper detects among 100 languages when the hint is unset, which is the right
default for a bot that hears more than one language. Pinning it to `en` is
usually a mistake if anyone ever speaks anything else: the audio is still heard,
but the words come back as English, and a misheard song title is a wrong song.

Whisper large-v3-turbo is multilingual, where the Parakeet it replaced was
English-only. It is much slower on the CPU — ~17 s per short utterance against
~0.7 s, or ~1 s once the GPU is doing it. Set `PARAKEET_MODEL` back to a
Parakeet id if you only ever speak English and want the lower latency.

### Environment variables

| Variable          | Required | Description                                        |
| ----------------- | -------- | -------------------------------------------------- |
| `DISCORD_TOKEN`   | yes      | Bot token from the Discord developer portal        |
| `VOICE_CHANNEL_ID`| yes      | ID of the voice channel the bot watches and joins  |
| `ASR_ENGINE`      | no       | `direct` (default, in-process transcribe.cpp) or `handy` (subprocess) |
| `PARAKEET_MODEL`  | no       | The large model, loaded on a wake word: a `.gguf` path or Hugging Face model id (default Whisper large-v3-turbo Q8_0) |
| `WAKE_MODEL`      | no       | The small always-on model: a `.gguf` path or Hugging Face model id (default Parakeet TDT+CTC 110M) |
| `ASR_LANGUAGE`    | no       | One ISO-639-1 language code, e.g. `en` or `pt`. Unset means the model detects it. `pt-PT` is not valid |
| `WAKE_WORDS`      | no       | Comma-separated wake words (default `bot,play,ut,ot`) |
| `LLM_ENDPOINT`    | no       | Ollama-compatible endpoint that rewrites a misheard transcript. Unset by default, which uses the words as heard |
| `LLM_MODEL`       | no       | Model to ask at that endpoint (default `llama3.2`) |
| `ALERT_CHANNEL_ID`| no       | Text channel for the now-playing dashboard and every other bot message (defaults to the server's system channel, then to the first text channel) |
| `MATCH_THRESHOLD` | no       | Lowest score a candidate may reach and still be played without asking, in points (default `90`) |
| `SEARCH_CANDIDATES` | no     | How many YouTube results a request is scored against, 1–50 (default `8`) |
| `AMBIGUOUS_POLICY` | no | `ask` (default), `auto` (play the best match without asking; same-title recordings are settled by plays), or `popular` (settle a shared title by popularity) |

A malformed value is a startup error rather than a silent default, so a typo in
a threshold is reported instead of quietly reverting to the default. Each
threshold is range-checked against the range its setting documents.

Secrets live in a gitignored `.env` file next to the binary, loaded through
[`dotenvy`](https://crates.io/crates/dotenvy). Real environment variables take
precedence over the file.

Wake words longer than four letters tolerate one misheard character, so a
custom word like "shamash" would still answer to "shemash" and "shammash".
Shorter ones must be heard exactly, otherwise ordinary speech ("not", "boy")
would trigger the bot — which is why "ut" and "ot" are listed alongside "bot":
speech-to-text often drops the first consonant of a short word.

### Tuning the wake word

The always-on gate is the part most likely to need adjusting on a real voice,
and `examples/transcribe.rs` is the instrument for it (see below). Three
constants in `src/wake.rs` decide how it behaves:

| Constant | Default | Raise it to |
| --- | --- | --- |
| `WAKE_WINDOW` | 2 s | hear a slower wake word, at the cost of more to transcribe each check |
| `WAKE_HOP` | 500 ms | use less CPU on a busy channel, at the cost of missing more of a quick shout |
| `WAKE_TAIL_FRAMES` | 500 ms | hear a wake word at the *end* of a sentence rather than the start |

`WAKE_HOP` is the CPU dial. Each check is one inference on the small model, so a
busy channel costs about two inferences a second.

## Run

```sh
cargo run --release
```

### The rewrite stage

Off by default, and deliberately: the catalog already corrects most mishearing
for free and returns the track's real running time, so the rewrite has to earn
its place on the requests the catalog cannot rescue.

Speech-to-text renders a title and an artist as whatever letters it hears —
"hoolig bts" for "Hooligan, BTS" — and everything downstream works from those
letters. A music catalog can only match what it is given, so a title mangled
past recognition can never be found no matter how good the matching is. A small
instruction model is good at exactly that guess.

Set `LLM_ENDPOINT` to switch it on:

```sh
LLM_ENDPOINT=http://localhost:11434
LLM_MODEL=llama3.2
cargo run --release
```

It is one HTTP request per command, bounded at three seconds, and every way it
can fail — endpoint down, model not pulled, reply that is not a song request,
reply that is an explanation rather than an answer — falls back to the words that
were actually heard. It never becomes the reason a request goes unplayed.

### How a request becomes a song

Speech-to-text mishears proper nouns, so a request arrives as something like
`hoolig bts` and a flat search hands back whatever matched those letters. Fixing
that with more string matching against YouTube titles is guessing, because
uploader-written titles are not a list of songs.

A music catalog *is* a list of songs. Before anything is searched for, the
request is resolved against YouTube Music's songs search — one unauthenticated
POST, no key or account — which returns the artist's own spelling of the title,
the track's real running time, and the video id that carries its audio. The
names are fuzzy-matched against the transcript (lowercased, punctuation and
"feat."/"remastered" noise stripped), and a match that does not clear the
matcher's threshold is discarded in favour of the words that were actually
heard.

The lookup is bounded at two seconds. It is an improvement on the raw transcript
and must never be the reason a command goes unanswered, so a slow or failing
catalog call falls back to searching the transcript rather than blocking.

A YouTube search returns whatever matched the words, and for a popular song that
routinely includes other people's takes of it. Ranking those by view count picks
the official video when the album track was meant, and a remix when the original
was. A search for *Tame Impala Dracula* puts this plainly:

| result                                  | length | views |
| --------------------------------------- | ------ | ----- |
| Official Video                          | 234 s  | 75.5 M |
| JENNIE Remix (Lyrics)                   | 211 s  | 50.8 M |
| **Dracula**, artist's channel           | **206 s** | 2.8 M |

The catalog says the song runs 205 s, so the two rows above it are not that song.

So the request is resolved against a music catalog first, which supplies the
canonical artist, title and running time; the YouTube search is built from
those names, and the results are scored against that running time rather than
by popularity. Popularity is kept, log-scaled and capped, only to break ties
between equally good uploads.

Below `MATCH_THRESHOLD` the bot does not guess. It posts the three closest
results in the alert channel as numbered buttons and waits. With
`AMBIGUOUS_POLICY=auto`, a catalog match that is real but not decisive — two
artists with the same title, say — plays the most-played recording of that
title instead, so a spoken request can finish without a button press.

When the catalog resolved nothing at all, *nothing* is played unasked — the
threshold only decides the order the options are offered in. Without a verified
running time the scorer has a title similarity and a view count and nothing else,
and that is not enough to tell the studio recording from a remix of it.

Some titles are simply not distinctive. "Look at Me" has been recorded by
Naughty Boy, Tems, The Neighbourhood and others, and a request that named no
artist gives the catalog nothing to tell them apart — every one of them scores
identically. Naming one of those artists in the YouTube query would aim the
search at a song the listener never asked for, so the bot does not: it keeps the
title, drops the guessed artist, and offers the choices instead.

" by " belongs to more song titles than it separates artists — *Stand By Me*,
*Take Me By The Hand*, *By The Way* — so splitting on the last one is a guess and
only a guess. The bot keeps every reading of what was said and tries each in
turn, under one shared time budget: "stand by me by ben e king" is first tried
as *Ben E. King — Stand By Me*, and as *Stand* by *Me by Ben E King* only if
that finds nothing.

By default the bot logs only what it heard, what it did about it, and which
models loaded:

```
INFO shamash::direct: loaded ASR model model=... backend=Vulkan0 device=Vulkan0
INFO shamash::bot: Shamash starting up request_model=... wake_model=... wake_words=bot,play,ut,ot
INFO shamash::pipeline: request heard guild_id=... transcript="bot play dracula by tame impala"
INFO shamash::pipeline: playing a request guild_id=... request="dracula by tame impala"
```

Serenity and songbird log a line per connection and per packet, which drowns
those out, so they are silenced at info. Warnings still come through from
everything, so a failure is never silent.

One warning is dropped outright:

```
WARN songbird::driver::tasks::udp_rx: RTCP decryption failed: Crypto(Error)
```

Discord sends RTCP reports for streams the bot has no key for, so this fires
continuously on a perfectly working connection, and there is no songbird setting
to turn it off. It is matched by message rather than by module, because
`RTP decryption failed` and `Decode error for SSRC` come from the same module
and both mean the bot has stopped hearing.

Set `RUST_LOG=shamash=debug` to see what the wake model heard on each check, the
resolution, the scores and the runner-up:

```
wake model heard: "shamash play dracula"
resolved against the catalog catalog=Dracula — Tame Impala
  duration=Some(205.0) confidence=0.94 query=Tame Impala Dracula
chose a video heard=drakula tame impala picked=Dracula channel=Tame Impala
  chosen=total 124.0 (duration +100.0, channel +30.0, relevance +18.0,
  media +0.0, views +6.0) runner_up=Dracula (Lyrics) (101.4)
  threshold=90.0 confident=true
```

### When it cannot find the song

A request that turns into nothing used to be a `println!` and silence: the
person who spoke heard no music and no explanation, and could not tell a bad
match from a bot that had not heard them. Now every failure is a **lower tone**
(440 Hz, an octave below the wake acknowledgement) followed by a message in the
alert channel that says what was heard:

> I heard “dracula by tame impala”, but couldn’t match it to a song. Try “play
> \<title\> by \<artist\>”.

There are three: no match, a download that failed, and a queue with no room.

### `/play`

```
/play dracula
```

Searches the catalog for what you typed and posts the results as numbered
buttons, nine to a page:

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

**Next page** shows the next nine from the *same* search — every page is a
window onto one list, so turning the page never asks the catalog again and
never reshuffles what you have already looked at. It is disabled on the last
page rather than removed, because a button that vanishes makes the message look
broken.

Speech asks the same question through the same chooser. When the bot is
confident about what it heard it just plays, as before; when it is not, or when
the catalog has never heard of the song, it posts this same paged chooser
instead of a separate short list. The difference is only who may answer: a
`/play` chooser belongs to whoever typed it, while one raised by speech is the
voice channel's, because the bot hears a voice and cannot tell who spoke.

Picking a song finds its upload and queues it. The buttons name *songs*, not
videos: that is the order that makes a second page worth having, because a
catalog has far more songs under a query than there are uploads worth offering.

Unlike the dashboard, a `/play` chooser is private to whoever typed it — it
answers one person's question rather than controlling a shared queue, so anyone
else who presses gets the same "join the voice channel" refusal. Running
`/play` itself needs you to be in the bot's voice channel, since that is the
only place it can play anything.

### Controlling playback

The dashboard in the alert channel is the only shared control surface:

| Button   | Effect                                                     |
| -------- | ---------------------------------------------------------- |
| Pause    | Pauses the current song; the button becomes Resume         |
| Resume   | Carries on from where the song paused                        |
| Skip     | Drops the current song and starts the next one queued       |
| Queue    | Lists what is waiting, soonest first                        |
| Repeat   | Replays the current song when it ends with nothing queued after it; the button reads `Repeat: on` while it is set |
| Stop     | Stops playback and clears the queue                         |

**Queue** swaps the dashboard for a list of the waiting songs and leaves a
**Back** button to return. The dashboard is one message edited in place, so it
remembers which of the two it is showing: a song ending while you are reading
the queue leaves you reading the queue.

**Repeat** is repeat *one*: a song that ends with nothing queued behind it plays
again, rather than the queue starting over. It does not interrupt the list — a
song with others waiting still plays the next one.

When the bot cannot tell which song was meant, it posts the three closest
results in the same channel as numbered buttons and plays nothing. Each option
shows the artist, the running time and its play count — YouTube Music's plays
for a catalog song, the upload's views for an unverified result — so two
recordings of one title can be told apart. Pressing one plays that song; the
others stay selectable until one is used.

Only members **currently in the voice channel the bot is playing in** can press
any of these, or run `/stop`. Everyone else gets an ephemeral "join the voice
channel first" and nothing happens. The dashboard is a text-channel message, so
without that check anyone who could read it could stop someone else's music.

`/stop` is separate: it also leaves the voice channel, so the bot stops
transcribing until someone comes back.

Each song is cached under its own video id, so a request can never be served
the audio of an earlier one. The newest few downloads are kept and the rest
pruned, which keeps a repeated request instant and stops the cache growing
without bound.

A song usually does not wait to be cached before it plays. One yt-dlp
extraction says which formats the video publishes, and the one symphonia can
decode — the AAC in an MP4; symphonia 0.5 has no Opus decoder — is played
straight off YouTube while the rest of it is written to the cache in the
background. That is what moves the first sample from "after the whole file" to
"after the first few kilobytes": on a ten-song cold benchmark the median drops
from about 2.2 s to about 1.8 s, and the songs that stream need only ~8 KiB to
start.

Streaming is the fast path, not the only one. The URL is signed and expires,
so before playback the bot asks for the same range playback will ask for: a
403, or a connection that delivers nothing within 2.5 s, and the song is
downloaded and played from the cache instead. Each request logs which path ran
and how long it took to first audio.

## Testing without Discord

Record yourself saying a wake word, pausing, then *"bot, play Dracula by Tame
Impala"* — the pause matters, because it is where the bot's two tones fall:

```sh
cargo run --release --example transcribe -- recording.wav
```

This drives the real wake gate rather than a copy of it, so it measures the two
things worth measuring: whether the small model can hear the wake word at all,
and whether the large model turns what follows it into a playable request. It
prints every window the small model was given and what it heard:

```
wake model heard: "so what do you think"
wake model heard: "shamash play dracula"
>>> wake word heard after 3 check(s), in 1.84 s of audio
larger model ready in 412ms
>>> captured 1.62 s of request
request heard: "dracula by tame impala"
play: dracula by tame impala
```

Models and wake words come from the same settings as the bot, so a change to
`WAKE_MODEL` or `WAKE_WORDS` is measured by re-running this. `PARAKEET_MODEL`
and `WAKE_MODEL` both default to the Hugging Face cache.

It also prints a level report: the loudest 20 ms frame against the
voice-activity gate. If that frame sits below the gate, the bot cannot hear you
at all, so the report is the first thing to check when it stays silent.
`VAD_RMS_THRESHOLD` in `src/listener.rs` sets the gate; it defaults to 0.02,
which is -34 dBFS against a typical conversational voice at -20 dBFS.

If the wake word is never reported, the harness says how many checks were made:
zero means the model never saw enough speech, and a non-zero count means the
wake word is in there but not being recognised — which is a `WAKE_WORDS` or
`WAKE_HOP` problem rather than a level one.

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
listens with. Before it existed here, this bot transcribed through the
[Handy](https://github.com/cjpais/Handy) app as a subprocess, which cost a Tauri
startup and a full model reload on *every* utterance. Linking transcribe.cpp
directly and holding the model resident was the single biggest change in how
this bot performs — roughly 350 ms per command down to inference alone — and
turning on ggml's Vulkan backend afterwards took a Whisper decode from ~17 s to
~1 s on this machine.

That is not a small thing to hand someone. The library covers 16+ model
families behind one API, it keeps the model resident so a caller can use it the
way the caller actually wants to, and it did all of that while leaving room for
the app's own catalogue to keep working alongside it. It has been a great
building block, and the kind of dependency that makes a project like this
possible at all.

The model catalogue and the GGUF conversions come from the same group too, which
is why both defaults resolve straight out of their Hugging Face repositories.

If you are the maintainers: thank you, and please keep going.

## License

MIT — see [LICENSE](LICENSE).
