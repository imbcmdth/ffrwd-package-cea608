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

/// One caption service's state: what the live socket and the rows arguments
/// said, and what is still to be sent.
pub struct Captions {
    captioner: Captioner,
    schedule: Schedule,
    packer: Packer,
    /// The backlog, in seconds, that next earns a note.
    warn_at: f64,
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

    /// A row of rows argument `track`, applied at its own time.
    pub fn row(&mut self, track: usize, row: &Row) {
        self.schedule.row(track, row);
    }

    /// The SEI payload of the frame shown at `t` seconds, at `num`/`den`
    /// frames a second: every edit due by then applied, then as many pairs
    /// as the frame carries.
    pub fn frame(&mut self, t: f64, num: u32, den: u32) -> Vec<u8> {
        self.schedule.apply_due(t, &mut self.captioner);
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
            },
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
}
