# Shamash

A wake-word Discord music bot written in Rust.

Shamash sits in one voice channel, listens for its name, and plays the song
that is asked for:

> **"bot, play Dracula by Tame Impala"**

Speech recognition runs on your own machine through
[transcribe.cpp](https://github.com/handy-computer/transcribe.cpp), so no audio
leaves the host.

**Contents:** [Features](#features) · [Quick start](#quick-start) ·
[Documentation](#documentation) · [Development](#development) ·
[Acknowledgements](#acknowledgements)

## Features

- **Hands-free requests.** Say the wake word and a song, in one breath or in
  two. The music ducks to 20% while you speak.
- **Always on, and cheap.** Only a voice detector runs continuously. The small
  wake model runs while someone is talking, and the large model only on an
  utterance that had the wake word in it.
- **Finds the right upload.** A request is resolved against YouTube Music's
  song catalog, so the artist's own upload beats a remix or a re-upload. If it
  is not sure, it asks instead of guessing.
- **Remembers your answer.** Pick a song once from "Which one?" and the same
  words play it straight away next time. See
  [Song memory](docs/usage.md#song-memory).
- **`/play` with live suggestions.** Start typing and matching songs appear.
  Links, playlists and albums work too.
- **`/lock` and `/unlock`.** Turn listening off without removing the bot.
- **Autonomous.** Joins when someone enters the configured channel and leaves,
  clearing the queue, once nobody is left.
- **A queue and a dashboard.** A now-playing message with a progress bar and
  buttons to pause, skip, repeat, stop and edit the queue.
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
else is optional; see [Settings](docs/configuration.md#settings).

### 3. Run it

Either in Docker, which needs nothing else installed, or from source.

The two speech models, about 1 GB together, download on the first start and
are kept from then on. `MODEL_DOWNLOAD=off` turns that off, for a machine that
must not reach out; then fetch them yourself (see
[The two models](docs/configuration.md#the-two-models)).

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

## Documentation

| Page | What is in it |
| --- | --- |
| [Using Shamash](docs/usage.md) | Saying a request, slash commands, `/play`, the dashboard and queue, song memory |
| [Configuration](docs/configuration.md) | Every setting, the two speech models, wake words, the rewrite stage, tuning |
| [How it works](docs/how-it-works.md) | From hearing a request to playing the song |
| [Troubleshooting](docs/troubleshooting.md) | Logs, common problems, testing without Discord |

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
