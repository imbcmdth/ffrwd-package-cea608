# ffrwd/cea608

Closed captions inside a stream's own H.264 or HEVC. `captions` puts
CEA-608 roll-up captions into every access unit an encoder writes, as the
ATSC A/53 SEI message that players read, so a player's CC button, YouTube
and Twitch all show them, and every remux after the encoder carries them
with the picture.

Requires ffrwd 0.29 or later, whose `ffrwd/wasm` is 0.19.1.

## Captions from rows of speech

For a file, the text comes from another module's rows: a parakeet or
whisper `transcribe`, or anything else that writes cues.

```pgsql
COPY (
  SELECT ffrwd.cea608.captions(f.video[1],
           cues => ffrwd.parakeet.transcribe(f.audio[1]),
           prefix => '>>'),
         f.audio[1]
  FROM input('match.mp4') f
) TO 'captioned.mp4'
```

The picture is not decoded or encoded again: the file's own H.264 is
copied into the module and out to the new file, one SEI per access unit
richer.

Each row is a cue: `text`, `start_t` and `end_t`. A row with no `final`
field is a whole cue, as whisper writes them, and goes up as one at its
`start_t`. Rows with `final` are a streaming transcript, as parakeet
writes them: the utterance so far, again each time it grows, and once
more with `final` true when it ends. A new utterance starts a row of its
own at its `start_t`, with `prefix` in front of it. A growing one sends
only the words it added. The word a row ends with may be half written, so
each word waits until a later row shows it whole and then goes out at the
time it was said. A finished utterance stays on screen until the next.

`cues2` and `prefix2` are a second speaker. Each speaker's new utterance
starts a row, and so do more words from a speaker whose row someone else
has taken since, so the marks show who is speaking:

```pgsql
SELECT ffrwd.cea608.captions(f.video[1],
         cues  => ffrwd.parakeet.transcribe(c1.audio[1]), prefix  => '>>',
         cues2 => ffrwd.parakeet.transcribe(c2.audio[1]), prefix2 => '<<')
```

The rows are paired with the packets by time, so the captions land on the
frames their words are spoken over.

## Live captions

For a live broadcast, the text comes from a process beside the query over a
loopback TCP socket, one JSON command a line:

```pgsql
COPY (
  SELECT ffrwd.cea608.captions(t.video[1], port => 9305), t.audio[1]
  FROM input('srt://0.0.0.0:9000?mode=listener', realtime => true) t
) TO 'udp://127.0.0.1:5000' WITH (format 'mpegts', video_codec 'h264_nvenc')
```

```
$ printf '{"break": true, "say": ">> WHAT A PLAY"}\n{"say": " FROM RED"}\n' | nc 127.0.0.1 9305
```

- `{"say": "TEXT"}` adds text to the row on screen exactly as written, so
  the space between two says is the sender's: `{"say": " MORE"}`. A say
  that starts without a space continues the word the row ends with.
- `{"break": true}` starts a new row for the text that follows. A break
  with nothing after it sends nothing.
- `{"clear": true}` erases the captions on screen.

One object may carry several of them, which apply as clear, then break,
then say. A line may also be an array of objects. Fields the module does
not know are ignored. A command goes into the next frame the module
handles, and its first letter is on air within one 608 pair (33 ms) on
the row it continues, or six pairs (0.2 s) when it starts a new row.

One connection at a time; a new one replaces the old, so a sender that
reconnects is never locked out. Behind an encoder the sidecar hosts, as
`h264_nvenc` is in a region that composes the picture on the GPU, the
module reads the encoder's packets from the region directly.

## What goes into the stream

The captions are CC1, roll-up on the bottom rows, `roll_up` rows of them
showing (2 by default, 3 or 4 when asked). A row is 32 columns, and a word
that does not fit starts the next row. Each row starts with Resume Roll-Up,
a carriage return and a preamble for the bottom row, so a player that
joins the stream mid-broadcast shows captions from the next row on. Every
control code goes twice, as 608 asks. `channel` picks CC2, CC3 or CC4
instead.

Text goes out in 608's basic characters where it can. The special set
(`♪`, `½`, `®`) goes as itself, and so does the extended set (`É`, `—`,
`“`) behind a plain stand-in (`E`, `-`, `"`) that a player without the set
keeps. Other characters are spelled plainly where they have an obvious
spelling (`…` is `...`) and dropped where they have none.

Every access unit gets one `user_data_registered_itu_t_t35` SEI (country
0xB5, provider 0x0031, `GA94`, `cc_data`), before its first coded slice,
with nothing else in the packet moved. How much a frame carries follows the
stream's frame rate:

| frame rate | `cc_count` | 608 pairs a frame |
|---|---|---|
| 60, 59.94 | 10 | one, field 1 and field 2 taking turns |
| 50 | 12 | one, taking turns |
| 30, 29.97 | 20 | a field 1 pair and a field 2 pair |
| 25 | 24 | two |
| 24, 23.976 | 25 | three, field 1 and field 2 taking turns |

A frame with nothing to say carries null pairs marked not valid and DTVCC
padding, so the caption data never stops. At 30 and 60 fps field 1 carries
line 21's 29.97 pairs a second, about 60 characters; at 25 and 50 it is 25
pairs, and at 24 it is 36. Text that arrives faster waits and goes out
late, with a line on stderr once five seconds are waiting. Past two
minutes waiting, new text is dropped, with a line saying so.

A frame that already carries A/53 captions from its encoder is left as it
is. The packets can be framed with start codes or with lengths, as the
stream's extradata says. They leave in the order they came, with their
timestamps; each is held until the stream's B-frames have settled which
frame is shown next, since captions have to be given out in presentation
order. That is two frames for `libx264` and `h264_nvenc` at their defaults
and none for a stream without B-frames.

At the end of the stream the module writes one summary row (frames
captioned, bytes added, commands and rows read, the time spent and how long
packets were held) and the same as a line on stderr.

## Exports

- `captions(v video_stream, cues cue[] DEFAULT NULL, cues2 cue[] DEFAULT
  NULL, port number DEFAULT NULL, prefix text DEFAULT NULL, prefix2 text
  DEFAULT NULL, roll_up number DEFAULT 2, channel number DEFAULT 1)`
  returns `packets`: `v`'s own packets with CEA-608 captions in them.

## Building

```
cargo build --target wasm32-wasip2 --release
cargo test
```

`cargo build --target wasm32-wasip2 --release -p captions --features
trace` builds a module that writes a line on stderr for each live command,
with the frame it went into, for measuring where commands land.

## Licence

MIT. The 608 code tables and the `cc_data` writer are Matthew Waters'
`cea608-types` and `cea708-types` (MIT); the NAL and SEI layer is
`ffrwd-nal` (Apache-2.0).
