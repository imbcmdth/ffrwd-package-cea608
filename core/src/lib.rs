//! CEA-608 roll-up captions into an H.264 or HEVC stream's own access units,
//! as ATSC A/53 `cc_data` in a T.35 SEI: what players, YouTube and Twitch
//! read as the stream's captions.
//!
//! - [`command`]: the live protocol, one JSON command a line.
//! - [`cues`]: rows of speech (whole cues or a streaming transcript) as
//!   roll-up edits at their times.
//! - [`captioner`]: the roll-up writer, text in and 608 pairs out.
//! - [`chars`]: which 608 set each character goes out in.
//! - [`pack`]: a frame's pairs as `cc_data` at its frame rate.
//! - [`splice`]: the SEI into an access unit.
//! - [`order`]: access units in presentation order.
//!
//! [`Captions`] ties the first five together for the module.

pub mod captioner;
pub mod chars;
pub mod command;
pub mod cues;
pub mod order;
pub mod pack;
pub mod splice;

use captioner::{Captioner, Service};
use command::Command;
use cues::{Row, Schedule};
use pack::{t35_payload, Packer};

/// The most text a burst may queue, in seconds of 608 time. Past it a
/// runner is sending faster than speech ever runs, and the new text is
/// dropped with a note rather than growing the queue for good.
pub const MAX_BACKLOG_SECONDS: f64 = 120.0;

/// How late a row's text may go out, in seconds. A row stamped further
/// behind the frame last captioned than this arrives after the picture it
/// belongs to has gone, and is dropped; an edit is dropped while this much
/// text is already waiting to go out ahead of it. A producer that stalled
/// and then hands over its rows all at once, which a publisher started
/// again behind a reader can do, would otherwise queue minutes of 608.
pub const MAX_LATE_SECONDS: f64 = 10.0;

/// One caption service's state: what the live socket and the rows arguments
/// said, and what is still to be sent.
pub struct Captions {
    captioner: Captioner,
    schedule: Schedule,
    packer: Packer,
    /// The backlog, in seconds, that next earns a note.
    warn_at: f64,
    /// The time of the frame last captioned.
    shown: Option<f64>,
    /// Rows dropped since the last one taken, and the furthest behind of
    /// them, in seconds; a note goes out when the drops start and end.
    late: Option<(u64, f64)>,
    /// Edits dropped behind a full queue since the queue last had room.
    crowded: u64,
    /// Rows and edits dropped, all told.
    dropped: (u64, u64),
    notes: Vec<String>,
}

impl Captions {
    /// Captions on `service`, rolling up `roll_up` rows, with one track per
    /// rows argument and the marker each starts its rows with.
    pub fn new(service: Service, roll_up: u8, prefixes: Vec<Option<String>>) -> Captions {
        Captions {
            captioner: Captioner::new(service, roll_up),
            schedule: Schedule::new(prefixes),
            packer: Packer::new(service),
            warn_at: 5.0,
            shown: None,
            late: None,
            crowded: 0,
            dropped: (0, 0),
            notes: Vec::new(),
        }
    }

    /// A command from the live socket, applied now.
    pub fn live(&mut self, command: Command) {
        match command {
            Command::Say(text) => {
                if self.captioner.backlog_seconds() > MAX_BACKLOG_SECONDS {
                    self.notes.push(format!(
                        "dropped {:?}: {:.0} s of captions are already waiting",
                        text,
                        self.captioner.backlog_seconds()
                    ));
                    return;
                }
                if self.captioner.owner().is_none() {
                    self.captioner.start_row_for(None);
                }
                self.captioner.append(&text);
            }
            Command::Break => self.captioner.start_row_for(None),
            Command::Clear => self.captioner.clear(),
        }
    }

    /// A row of rows argument `track`, stamped `at` seconds, applied at its
    /// own time; one stamped more than [`MAX_LATE_SECONDS`] behind the frame
    /// last captioned is dropped.
    pub fn row(&mut self, track: usize, row: &Row, at: f64) {
        let behind = self.shown.map_or(0.0, |shown| shown - at);
        if behind > MAX_LATE_SECONDS {
            self.dropped.0 += 1;
            let (count, worst) = self.late.get_or_insert_with(|| {
                self.notes.push(format!(
                    "rows are arriving {behind:.1} s behind the picture, and those more than {MAX_LATE_SECONDS:.0} s behind are dropped"
                ));
                (0, 0.0)
            });
            *count += 1;
            *worst = worst.max(behind);
            return;
        }
        if let Some((count, worst)) = self.late.take() {
            self.notes.push(format!(
                "dropped {count} rows that arrived more than {MAX_LATE_SECONDS:.0} s behind the picture, the furthest {worst:.1} s behind"
            ));
        }
        self.schedule.row(track, row);
    }

    /// The SEI payload of the frame shown at `t` seconds, at `num`/`den`
    /// frames a second: every edit due by then applied, then as many pairs
    /// as the frame carries.
    pub fn frame(&mut self, t: f64, num: u32, den: u32) -> Vec<u8> {
        self.shown = Some(self.shown.map_or(t, |shown| shown.max(t)));
        let crowded = self
            .schedule
            .apply_due(t, &mut self.captioner, MAX_LATE_SECONDS);
        if crowded > 0 && self.crowded == 0 {
            self.notes.push(format!(
                "{:.1} s of captions are waiting; row text that would wait longer is dropped",
                self.captioner.backlog_seconds()
            ));
        }
        self.crowded += crowded;
        self.dropped.1 += crowded;
        if crowded == 0 && self.crowded > 0 && self.captioner.backlog_seconds() < MAX_LATE_SECONDS {
            self.notes.push(format!(
                "dropped {} edits of row text behind a full queue",
                self.crowded
            ));
            self.crowded = 0;
        }
        let backlog = self.captioner.backlog_seconds();
        if backlog >= self.warn_at {
            self.notes.push(format!(
                "{backlog:.1} s of captions are waiting: text is arriving faster than 608 carries it"
            ));
            self.warn_at *= 2.0;
        } else if backlog < 1.0 {
            self.warn_at = 5.0;
        }
        t35_payload(&self.packer.cc_data(&mut self.captioner, num, den))
    }

    /// Rows dropped for arriving too far behind the picture, and edits
    /// dropped behind a full queue.
    pub fn dropped(&self) -> (u64, u64) {
        self.dropped
    }

    /// Edits whose time has not come, and pairs not yet sent: what the end
    /// of the stream cut off.
    pub fn unsent(&self) -> (usize, usize) {
        (
            self.schedule.len(),
            self.captioner.backlog() + self.packer.queued(),
        )
    }

    /// What is worth a line on stderr since the last call.
    pub fn notes(&mut self) -> Vec<String> {
        std::mem::take(&mut self.notes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cea708_types::CCDataParser;

    /// The field 1 text a run of frames carries, `|` for each carriage
    /// return, decoded the way a player does.
    fn decode(payloads: &[Vec<u8>]) -> String {
        let mut parser = CCDataParser::new();
        parser.handle_cea608();
        let mut state = cea608_types::Cea608State::default();
        let mut out = String::new();
        for payload in payloads {
            parser
                .push(&payload[pack::T35_HEADER.len()..payload.len() - 1])
                .unwrap();
            for pair in parser.cea608().unwrap().to_vec() {
                if let cea708_types::Cea608::Field1(a, b) = pair {
                    match state.decode([a, b]).unwrap() {
                        Some(cea608_types::Cea608::Text(t)) => {
                            if t.needs_backspace {
                                out.pop();
                            }
                            out.extend(t.char1);
                            out.extend(t.char2);
                        }
                        Some(cea608_types::Cea608::CarriageReturn(_)) => out.push('|'),
                        Some(cea608_types::Cea608::EraseDisplay(_)) => out.push('#'),
                        _ => {}
                    }
                }
            }
        }
        out
    }

    #[test]
    fn live_commands_come_out_as_rows() {
        let mut c = Captions::new(Service::cc(1).unwrap(), 2, Vec::new());
        c.live(Command::Say(">> HELLO".into()));
        c.live(Command::Say(" THERE".into()));
        c.live(Command::Break);
        c.live(Command::Say("<< HI".into()));
        c.live(Command::Clear);
        let frames: Vec<Vec<u8>> = (0..120).map(|i| c.frame(i as f64 / 60.0, 60, 1)).collect();
        assert_eq!(decode(&frames), "|>> HELLO THERE|<< HI#");
        assert_eq!(c.unsent(), (0, 0));
    }

    #[test]
    fn rows_wait_for_their_time() {
        let mut c = Captions::new(Service::cc(1).unwrap(), 2, vec![None]);
        c.row(
            0,
            &Row {
                text: "LATER".into(),
                start_t: 1.0,
                end_t: Some(1.5),
                finished: None,
                voice: None,
            },
            0.0,
        );
        // A new row starts its control codes early, so that its first
        // letter goes out on the row's time.
        let early: Vec<Vec<u8>> = (0..24).map(|i| c.frame(i as f64 / 30.0, 30, 1)).collect();
        assert_eq!(decode(&early), "");
        let mut frames = Vec::new();
        let mut first_letter = None;
        for i in 24..60 {
            frames.push(c.frame(i as f64 / 30.0, 30, 1));
            if first_letter.is_none() && decode(&frames).contains('L') {
                first_letter = Some(i);
            }
        }
        assert_eq!(decode(&frames), "|LATER");
        assert_eq!(
            first_letter,
            Some(30),
            "the first letter on the row's frame"
        );
    }

    #[test]
    fn a_burst_is_queued_and_noted_never_lost() {
        let mut c = Captions::new(Service::cc(1).unwrap(), 2, Vec::new());
        let words: Vec<String> = (0..100).map(|i| format!("W{i:03}")).collect();
        c.live(Command::Say(words.join(" ")));
        let mut frames = Vec::new();
        for i in 0..60 * 20 {
            frames.push(c.frame(i as f64 / 60.0, 60, 1));
        }
        assert!(!c.notes().is_empty(), "the backlog was noted");
        let text = decode(&frames).replace('|', " ");
        let got: Vec<&str> = text.split_whitespace().collect();
        assert_eq!(got, words);
    }

    fn cue(text: &str, start_t: f64) -> Row {
        Row {
            text: text.into(),
            start_t,
            end_t: Some(start_t + 0.5),
            finished: None,
            voice: None,
        }
    }

    #[test]
    fn a_row_far_behind_the_picture_is_dropped_and_one_near_it_goes_out_late() {
        let mut c = Captions::new(Service::cc(1).unwrap(), 2, vec![None]);
        for i in 0..60 * 30 {
            c.frame(i as f64 / 60.0, 60, 1);
        }
        let shown = (60 * 30 - 1) as f64 / 60.0;
        c.row(0, &cue("STALE", 12.0), 12.5);
        c.row(0, &cue("ALSO STALE", 15.0), 15.5);
        c.row(0, &cue("LATE", 26.0), 26.5);
        let notes = c.notes();
        assert_eq!(notes.len(), 2, "{notes:?}");
        assert!(notes[0].contains("17.5 s behind"), "{notes:?}");
        assert!(notes[1].contains("dropped 2 rows"), "{notes:?}");
        assert!(notes[1].contains("17.5 s behind"), "{notes:?}");
        assert_eq!(c.dropped(), (2, 0));
        let frames: Vec<Vec<u8>> = (0..60)
            .map(|i| c.frame(shown + (i + 1) as f64 / 60.0, 60, 1))
            .collect();
        assert_eq!(
            decode(&frames),
            "|LATE",
            "the row near the picture goes out at once"
        );
    }

    #[test]
    fn a_catch_up_of_rows_queues_no_more_than_the_bound_of_608() {
        // A producer that stalled for a minute hands over a minute of
        // utterances at once, each on time by its stamp.
        let mut c = Captions::new(Service::cc(1).unwrap(), 2, vec![None]);
        for i in 0..240 {
            let t = i as f64 * 0.25;
            c.row(0, &cue(&format!("UTTERANCE NUMBER {i:03} SAYS MORE"), t), t);
        }
        let mut most: f64 = 0.0;
        let mut frames = Vec::new();
        for i in 0..60 * 90 {
            frames.push(c.frame(i as f64 / 60.0, 60, 1));
            most = most.max(c.captioner.backlog_seconds());
        }
        assert!(
            most <= MAX_LATE_SECONDS + 1.0,
            "{most:.1} s of 608 queued at most"
        );
        let (_, edits) = c.dropped();
        assert!(edits > 100, "{edits} edits dropped");
        let notes = c.notes();
        assert!(
            notes.iter().any(|note| note.contains("is dropped")),
            "{notes:?}"
        );
        // What went out is whole utterances, the first of them first.
        let text = decode(&frames);
        assert!(
            text.starts_with("|UTTERANCE NUMBER 000 SAYS MORE"),
            "{text}"
        );
    }

    #[test]
    fn live_commands_keep_their_own_bound() {
        let mut c = Captions::new(Service::cc(1).unwrap(), 2, Vec::new());
        let words: Vec<String> = (0..300).map(|i| format!("W{i:03}")).collect();
        c.live(Command::Say(words.join(" ")));
        for i in 0..60 * 30 {
            c.frame(i as f64 / 60.0, 60, 1);
        }
        assert_eq!(c.dropped(), (0, 0), "a burst of live text is not row text");
    }
}
