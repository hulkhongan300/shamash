[← Back to the README](../README.md)

# How it works

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

## Hearing

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

## Resolving the request

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

## Choosing the upload

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

## Playback and caching

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
