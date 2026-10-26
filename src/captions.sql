-- ffrwd/cea608: CEA-608 captions in a stream's own encoded packets.
--
-- `captions` rewrites the access units of an H.264 or HEVC stream on their
-- way out of the encoder, adding to each one an SEI carrying ATSC A/53
-- `cc_data`: the captions a player shows behind its CC button, and what
-- YouTube and Twitch read from an RTMP or SRT ingest. The same stream comes
-- back, packet for packet, with the same timestamps. Every access unit
-- carries the SEI, with padding where there is nothing to say, so the
-- captions survive a cut, a remux or a segmenter anywhere downstream.
--
-- The captions are roll-up at the bottom of the screen: rows of 32 columns,
-- `roll_up` of them showing, a word that does not fit starting the next row.
-- 608 carries about 60 characters a second; text that arrives faster waits
-- its turn and is sent late rather than lost.
--
-- THE TEXT comes two ways, and one call may take both.
--
-- `cues` and `cues2` are rows of speech: a whisper `transcribe`'s cues, or a
-- parakeet `transcribe`'s rows. Each new utterance starts a row of its own at
-- its `start_t`, with `prefix` (or `prefix2`) in front of it, such as `>>`
-- for a change of speaker. A streaming transcript that re-sends its
-- utterance as it grows sends only the words it added, each once it is
-- whole and at the time it was said, and a finished utterance stays until
-- the next. Two arguments are two speakers, each starting rows of its own.
-- The rows are paired with the packets by time, which is the way for a
-- file:
--
--   COPY (
--     SELECT ffrwd.cea608.captions(f.video[1],
--              cues => ffrwd.parakeet.transcribe(f.audio[1]),
--              prefix => '>>'),
--            f.audio[1]
--     FROM input('match.mp4') f
--   ) TO 'captioned.mp4'
--
-- `port` is a loopback TCP port for live text, one JSON command a line,
-- applied the moment it arrives:
--
--   {"say": " WHAT A PLAY"}      text added to the row on screen as written
--   {"break": true}              the next text starts a new row
--   {"clear": true}              the captions on screen are erased
--
-- One connection at a time; a new one replaces the old.
--
--   COPY (
--     SELECT ffrwd.cea608.captions(t.video[1], port => 9305), t.audio[1]
--     FROM input('srt://0.0.0.0:9000?mode=listener', realtime => true) t
--   ) TO 'udp://127.0.0.1:5000' WITH (format 'mpegts', video_codec 'h264_nvenc')
--
-- `channel` is the caption service, CC1 to CC4. Frames the encoder already
-- put A/53 captions in are left as they are.
CREATE FUNCTION captions(v video_stream,
                         cues cue[] DEFAULT NULL,
                         cues2 cue[] DEFAULT NULL,
                         port number DEFAULT NULL,
                         prefix text DEFAULT NULL,
                         prefix2 text DEFAULT NULL,
                         roll_up number DEFAULT 2,
                         channel number DEFAULT 1)
RETURNS packets
  AS 'target/wasm32-wasip2/release/captions.wasm', 'captions' LANGUAGE wasm;
