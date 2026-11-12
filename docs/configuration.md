[← Back to the README](../README.md)

# Configuration

Settings come from the environment, or from the `.env` file next to the
binary. Real environment variables win over the file. A malformed value stops
the bot at startup instead of silently falling back to a default.

## Settings

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
| `LIBRARY_PATH` | `data/library.json` | Where picked songs are remembered (Docker: `/data/library.json`, in the volume). `off` remembers nothing |

## The two models

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

## Choosing wake words

Wake words longer than four letters tolerate one misheard character, so
"shamash" also answers to "shemash" and "shammash". Shorter ones must be heard
exactly, apart from one swapped vowel ("bot" heard as "but", which the small
model often does). Otherwise ordinary words like "not" or "boy" would wake it.

Avoid common words. "Play" as a wake word would make "let's play a game" a song
request.

## The rewrite stage

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

## Tuning

These constants in the code decide how listening behaves:

| Constant | Default | Raise it to |
| --- | --- | --- |
| `wake::WAKE_WINDOW` | 2 s | hear a slower wake word, at the cost of more audio per check |
| `wake::WAKE_HOP` | 250 ms | use less GPU on a busy channel, at the cost of a later first tone |
| `wake::ARM_TTL` | 8 s | give people longer to say the request after a bare wake word |
| `segment::HANGOVER_FRAMES` | 640 ms | survive longer pauses inside a request, at the cost of a slower answer |
| `player::DUCK_VOLUME` | 20% | lower the music less while someone talks to the bot |

Measure a change with the [offline harness](troubleshooting.md#testing-without-discord).
