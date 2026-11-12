[← Back to the README](../README.md)

# Troubleshooting

## Logs

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

## Common problems

| Symptom | Likely cause |
| --- | --- |
| No tone at all | Listening is locked (`/unlock`), or the wake word is not recognised. Check the debug log for what the wake model heard, and adjust `WAKE_WORDS` |
| A tone, then nothing plays | Look in the text channel: there is either a chooser or a message saying what was heard |
| The wrong song plays | Press **Wrong song** if the dashboard says it was remembered. Otherwise name the artist, and check `RUST_LOG=shamash=debug` for the scores |
| The first request after startup is slow | The request model loads on the first wake word. Later requests are fast |
| Requests take many seconds | The model is running on the CPU. Check the `backend=` in the startup log |
| `/play` suggestions do not appear | Restart the bot so it re-registers the command, then reload Discord |
| The build fails on `SPIRV-Headers` | Install the Vulkan packages, or drop the `vulkan` feature |

## Testing without Discord

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
