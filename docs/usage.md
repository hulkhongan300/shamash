[← Back to the README](../README.md)

# Using Shamash

How to talk to the bot, the slash commands, the dashboard, and what it does
when it is not sure which song you meant.

## Saying it

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

## Slash commands

| Command | What it does |
| --- | --- |
| `/play <song>` | Suggests songs as you type and queues the one you pick. A pasted YouTube or YouTube Music link plays that video, and a playlist or album link queues all of it |
| `/lock` | Stops listening. The bot stays in the channel and keeps playing |
| `/unlock` | Starts listening again |
| `/stop` | Stops playback, clears the queue and leaves the voice channel |

Every command needs you to be in the bot's voice channel.

## `/play`

Start typing after `/play` and the matching songs appear over the message box:

```
Tame Impala — Dracula (3:26)
Tame Impala — Dracula (feat. JENNIE) (3:30)
Iuly Neamtu — Dracula (Frappula) (2:17)
```

- **Pick a suggestion** and that exact song is queued. Each suggestion carries
  the video id YouTube Music lists for the song, so there is nothing left to
  search for.
- **Paste a YouTube link** and that video is queued. A link to a video
  inside a playlist (`watch?v=…&list=…`) is that one video.
- **Paste a playlist or album link** (`youtube.com/playlist?list=…`, the same
  on `music.youtube.com`, or a YouTube Music album page) and every track is
  queued in order. The first one is queued straight away and the reply says
  so; the rest follow in the background over the next few seconds. Tracks
  that do not fit in the 20-song queue are left off the end, and private or
  deleted ones are skipped. The reply says how many of each. Pressing Stop
  drops whatever has not been queued yet.

  Only `/play` does this. Saying a playlist or album to the bot is not
  supported: it cannot show what it is about to queue, and an album nobody
  meant to ask for is a lot of songs to skip.
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

## `/lock` and `/unlock`

`/lock` makes the bot drop everything it hears without running a model on it.
Playback, the dashboard and `/play` keep working. `/unlock` turns listening
back on. The reply is public, so the channel can see whether it is being
listened to. The lock is held in memory, so a restart starts listening again.

## The dashboard

The text channel has one now-playing message with the song's thumbnail, a
progress bar (`▬▬▬▬🔘▬▬▬▬▬▬▬ 1:12 / 3:26`) and these buttons:

| Button | Effect |
| --- | --- |
| Pause / Resume | Pauses or carries on with the current song |
| Skip | Starts the next song in the queue |
| Queue | Shows what is waiting, soonest first, where songs can be moved or removed, with a **Back** button |
| Repeat | Replays the current song when it ends with nothing queued after it |
| Wrong song | Only on a song played because it was remembered: forgets those words, skips the song and asks "Which one?" again |
| Stop | Stops playback and clears the queue |

A song asked for while another is playing waits in the queue, which holds up to
20 songs. Repeat is repeat *one*: it never stops the next queued song from
playing.

Discord messages do not update by themselves, so while a song plays the bot
edits the message every 15 seconds to move the bar along. Pausing freezes the
bar, and the edits stop while paused, while the queue is shown, and when
nothing is playing. A song whose length is unknown shows no bar.

In the queue view, pick a song from the menu, then press **Play next**, **Up**,
**Down** or **Remove**. The song stays picked after a move, so pressing **Up**
again keeps moving it. The view updates in place while the queue plays on, and
a button always acts on the song that was picked: if that song has started
playing or was removed in the meantime, the press does nothing. The song
playing now is not in the queue view; **Skip** is for that.

Only members currently in the bot's voice channel can press these buttons.
Everyone else gets a private "join the voice channel first" and nothing happens.

## Joining and leaving

The bot joins the configured voice channel when someone enters it, including at
startup. Once nobody is left in its channel (other bots do not count) it waits
15 seconds, in case someone only dropped out, then clears the queue and leaves.
That works even if someone dragged it into another channel.

## Song memory

When the bot asks "Which one?" and you pick a song, it remembers that the words
you said meant that song, so the same words play it straight away next time. It
works even when the artist you said is not the one credited ("All Caps by MF
DOOM" plays Madvillain).

- **Only your picks.** The bot never remembers its own guesses.
- **Said a little differently is fine.** "M.F. DOOM", a close mishearing like
  "MF Dume", or the title alone ("All Caps", when exactly one remembered song
  has that title) all find it. A phrase about as close to two remembered songs,
  or one with a different number in it ("Part 1" and "Part 2"), is asked about
  again instead.
- **Typed and spoken are one memory.** A pick from a typed `/play` search
  counts too, and the typed words are cleaned up the way spoken ones are. Teach
  it by typing and saying the words plays the song, and a typed search for
  words it already knows lists that song first. Links are never remembered.
- **Wrong song.** The dashboard marks a song played from memory with "🧠
  remembered from last time" and shows a **Wrong song** button. Pressing it
  forgets those words, skips the song and asks "Which one?" again, so the right
  pick is learned.
- **Two quick skips forget it too.** A remembered song skipped within 10
  seconds, twice, is forgotten.

The memory is kept in `LIBRARY_PATH` (see
[Settings](configuration.md#settings)); `LIBRARY_PATH=off` remembers nothing.


## When the bot is not sure

When the bot is not confident which song or upload was meant, it plays nothing.
Instead it posts the closest results in the text channel as the same paged
chooser `/play` uses. A spoken request's chooser belongs to anyone in the voice
channel, because the bot hears a voice and cannot tell who spoke.

If no catalog knows the song, the YouTube results are offered as *unverified*
choices, and nothing plays until someone presses one. See
[Choosing the upload](how-it-works.md#choosing-the-upload) for why.
