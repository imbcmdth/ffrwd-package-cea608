//! The roll-up writer: text, row breaks and clears in, 608 byte pairs out,
//! as fast as one field carries them.
//!
//! The captions are roll-up on the bottom rows of the screen. A row starts
//! with Resume Roll-Up, a carriage return and a preamble for the bottom row,
//! each sent twice: a decoder ignores a control pair it has just seen, so
//! the second copy is what survives a pair lost on the way. Starting every
//! row this way, not just the first, is what lets a viewer who joins
//! mid-stream see captions from the next row on; a decoder already in
//! roll-up on that row takes the repeats as no change.
//!
//! Text is word-wrapped at 32 columns, the width of a 608 row. Commands are
//! turned into codes as they arrive and queued, and each frame takes the
//! pairs its field has room for, so a burst of text is delayed rather than
//! lost.

use std::collections::VecDeque;

use cea608_types::tables::{
    Channel, Code, Color, Control, ControlCode, Field, PreambleAddressCode, PreambleType,
};

use crate::chars::{glyphs, Glyph};

/// The columns of a 608 row.
pub const COLUMNS: usize = 32;

/// The bottom row, counted from 0, where roll-up captions sit.
const BASE_ROW: u8 = 14;

/// The pairs one field carries a second: line 21 at 30000/1001 frames.
pub const PAIRS_PER_SECOND: f64 = 30000.0 / 1001.0;

/// The pairs a new row starts with before its first letter: roll-up, a
/// carriage return and a preamble, each twice.
pub const ROW_START_PAIRS: usize = 6;

/// One 608 caption service: CC1 and CC2 are field 1, CC3 and CC4 field 2.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Service {
    pub field: Field,
    pub channel: Channel,
}

impl Service {
    /// CC1 to CC4.
    pub fn cc(number: u8) -> Option<Service> {
        let (field, channel) = match number {
            1 => (Field::ONE, Channel::ONE),
            2 => (Field::ONE, Channel::TWO),
            3 => (Field::TWO, Channel::ONE),
            4 => (Field::TWO, Channel::TWO),
            _ => return None,
        };
        Some(Service { field, channel })
    }
}

/// What one queued slot is: a byte that may share its pair, or a pair of
/// its own.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Slot {
    Byte(u8),
    Pair([u8; 2]),
}

/// Who started the row on screen: a rows argument, or the live socket.
pub type Owner = Option<usize>;

pub struct Captioner {
    service: Service,
    roll_up: Control,
    queue: VecDeque<Slot>,
    /// Where the cursor will be once the queue is sent.
    column: usize,
    /// Whether the row the cursor is on has been started since the last
    /// clear: until it has, text waits behind a row start.
    row_started: bool,
    owner: Owner,
    /// Whether the last text appended ended in a space, which the next
    /// text's first word keeps.
    space_owed: bool,
}

impl Captioner {
    /// A writer for `service`, rolling up `rows` rows (2, 3 or 4).
    pub fn new(service: Service, rows: u8) -> Captioner {
        let roll_up = match rows {
            2 => Control::RollUp2,
            3 => Control::RollUp3,
            _ => Control::RollUp4,
        };
        Captioner {
            service,
            roll_up,
            queue: VecDeque::new(),
            column: 0,
            row_started: false,
            owner: None,
            space_owed: false,
        }
    }

    /// Who started the row text now lands on, while there is one.
    pub fn owner(&self) -> Option<Owner> {
        self.row_started.then_some(self.owner)
    }

    /// Words added to the row, one space before each unless the row is
    /// empty, wrapped onto a new row where the next word does not fit.
    pub fn say(&mut self, text: &str) {
        for word in text.split_whitespace() {
            self.word(word, true);
        }
        self.space_owed = false;
    }

    /// Text added to the row as written: a space where it has one, and
    /// none where it has none, so a piece that starts without one continues
    /// the word the row ends with. A run of spaces is one space, and a row
    /// never starts with one.
    pub fn append(&mut self, text: &str) {
        let spaced = self.space_owed || text.starts_with(char::is_whitespace);
        let mut any = false;
        for (index, word) in text.split_whitespace().enumerate() {
            self.word(word, index > 0 || spaced);
            any = true;
        }
        self.space_owed = if any {
            text.ends_with(char::is_whitespace)
        } else {
            spaced
        };
    }

    /// The next text starts a row of its own. Two breaks with nothing
    /// between them make one: roll-up has no use for an empty row.
    pub fn line_break(&mut self) {
        self.row_started = false;
        self.space_owed = false;
    }

    /// The text that follows starts a new row on behalf of `owner`.
    pub fn start_row_for(&mut self, owner: Owner) {
        self.line_break();
        self.owner = owner;
    }

    /// Erase what is on screen. The next text sets roll-up up again.
    pub fn clear(&mut self) {
        self.control(Control::EraseDisplayedMemory);
        self.row_started = false;
        self.column = 0;
    }

    /// How many pairs are waiting.
    pub fn backlog(&self) -> usize {
        let mut pairs = 0;
        let mut odd = false;
        for slot in &self.queue {
            match slot {
                Slot::Byte(_) if odd => odd = false,
                Slot::Byte(_) => {
                    pairs += 1;
                    odd = true;
                }
                Slot::Pair(_) => {
                    pairs += 1;
                    odd = false;
                }
            }
        }
        pairs
    }

    /// The seconds the waiting pairs take to send.
    pub fn backlog_seconds(&self) -> f64 {
        self.backlog() as f64 / PAIRS_PER_SECOND
    }

    /// The next pair to send, or None when there is nothing to say.
    pub fn next_pair(&mut self) -> Option<[u8; 2]> {
        match self.queue.pop_front()? {
            Slot::Pair(pair) => Some(pair),
            Slot::Byte(first) => {
                let second = match self.queue.front() {
                    Some(Slot::Byte(second)) => {
                        let second = *second;
                        self.queue.pop_front();
                        second
                    }
                    _ => NUL,
                };
                Some([first, second])
            }
        }
    }

    fn word(&mut self, word: &str, spaced: bool) {
        let glyphs: Vec<Glyph> = word
            .chars()
            .flat_map(|c| glyphs(c, self.service.channel))
            .collect();
        if glyphs.is_empty() {
            return;
        }
        if !self.row_started {
            self.start_row();
        }
        let space = usize::from(spaced && self.column > 0);
        if self.column + space + glyphs.len() > COLUMNS && self.column > 0 {
            self.start_row();
        } else if space == 1 {
            self.glyph(Glyph::Basic(Code::Space));
        }
        for glyph in glyphs {
            if self.column == COLUMNS {
                self.start_row();
            }
            self.glyph(glyph);
        }
    }

    fn start_row(&mut self) {
        self.control(self.roll_up);
        self.control(Control::CarriageReturn);
        self.control(Control::PreambleAddress(PreambleAddressCode::new(
            BASE_ROW,
            false,
            PreambleType::Color(Color::White),
        )));
        self.row_started = true;
        self.column = 0;
    }

    fn glyph(&mut self, glyph: Glyph) {
        match glyph {
            Glyph::Basic(code) => self.queue.push_back(Slot::Byte(byte(code))),
            Glyph::Special(code) => {
                let pair = pair(code);
                self.queue.push_back(Slot::Pair(pair));
                self.queue.push_back(Slot::Pair(pair));
            }
            Glyph::Extended { stand_in, code } => {
                self.queue.push_back(Slot::Byte(byte(stand_in)));
                let pair = pair(code);
                self.queue.push_back(Slot::Pair(pair));
                self.queue.push_back(Slot::Pair(pair));
            }
        }
        self.column += 1;
    }

    fn control(&mut self, control: Control) {
        let pair = pair(Code::Control(ControlCode::new(
            self.service.field,
            self.service.channel,
            control,
        )));
        self.queue.push_back(Slot::Pair(pair));
        self.queue.push_back(Slot::Pair(pair));
    }
}

/// The null byte with its parity bit: what fills the half of a pair that
/// has nothing in it.
const NUL: u8 = 0x80;

fn byte(code: Code) -> u8 {
    let mut out = [NUL; 2];
    code.write_into(&mut out);
    out[0]
}

/// A two byte code's pair. A special or extended character comes out of the
/// table without a field, and takes channel 1 or 2's first byte from its
/// own code.
fn pair(code: Code) -> [u8; 2] {
    let mut out = [NUL; 2];
    code.write_into(&mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use cea608_types::{Cea608, Cea608State, Mode};

    fn cc1() -> Captioner {
        Captioner::new(Service::cc(1).unwrap(), 2)
    }

    fn drain(c: &mut Captioner) -> Vec<[u8; 2]> {
        std::iter::from_fn(|| c.next_pair()).collect()
    }

    /// The commands a decoder makes of the pairs, duplicates dropped as a
    /// decoder drops them.
    fn decode(pairs: &[[u8; 2]]) -> Vec<Cea608> {
        let mut state = Cea608State::default();
        pairs
            .iter()
            .filter_map(|pair| state.decode(*pair).expect("valid parity"))
            .collect()
    }

    /// What the decoded commands put on the rows, oldest row first.
    fn screen(pairs: &[[u8; 2]]) -> Vec<String> {
        let mut rows = vec![String::new()];
        for command in decode(pairs) {
            match command {
                Cea608::Text(text) => {
                    let row = rows.last_mut().unwrap();
                    if text.needs_backspace {
                        row.pop();
                    }
                    row.extend(text.char1);
                    row.extend(text.char2);
                }
                Cea608::CarriageReturn(_) => rows.push(String::new()),
                Cea608::EraseDisplay(_) => rows = vec![String::new()],
                _ => {}
            }
        }
        rows.retain(|row| !row.is_empty());
        rows
    }

    fn odd_parity(byte: u8) -> bool {
        byte.count_ones() % 2 == 1
    }

    #[test]
    fn the_first_text_sets_roll_up_up_and_every_control_goes_twice() {
        let mut c = cc1();
        c.say("HI");
        let pairs = drain(&mut c);
        assert_eq!(
            pairs,
            vec![
                [0x94, 0x25], // RU2, CC1
                [0x94, 0x25],
                [0x94, 0xad], // CR
                [0x94, 0xad],
                [0x94, 0xe0], // PAC row 15, white
                [0x94, 0xe0],
                [0xc8, 0x49], // H I
            ]
        );
        assert!(pairs.iter().flatten().all(|b| odd_parity(*b)));
        let commands = decode(&pairs);
        assert_eq!(
            commands[0],
            Cea608::NewMode(Channel::ONE, Mode::RollUp2),
            "{commands:?}"
        );
        assert_eq!(commands.len(), 4, "the repeats are dropped: {commands:?}");
    }

    #[test]
    fn says_are_joined_by_one_space() {
        let mut c = cc1();
        c.say(">> GOOD");
        c.say("  CALL ");
        assert_eq!(screen(&drain(&mut c)), vec![">> GOOD CALL"]);
    }

    #[test]
    fn a_word_that_does_not_fit_goes_to_the_next_row() {
        let mut c = cc1();
        c.say("THE QUICK BROWN FOX JUMPS OVER THE LAZY DOG");
        let rows = screen(&drain(&mut c));
        assert_eq!(rows, vec!["THE QUICK BROWN FOX JUMPS OVER", "THE LAZY DOG"]);
        assert!(rows.iter().all(|row| row.len() <= COLUMNS));
    }

    #[test]
    fn a_word_exactly_as_long_as_the_row_fits_it() {
        let mut c = cc1();
        let word = "A".repeat(COLUMNS);
        c.say(&format!("{word} B"));
        assert_eq!(screen(&drain(&mut c)), vec![word, "B".to_owned()]);
    }

    #[test]
    fn a_word_longer_than_a_row_is_cut() {
        let mut c = cc1();
        let word = "X".repeat(40);
        c.say(&word);
        assert_eq!(screen(&drain(&mut c)), vec!["X".repeat(32), "X".repeat(8)]);
    }

    #[test]
    fn a_break_starts_a_row_when_text_follows_it() {
        let mut c = cc1();
        c.say(">> ONE");
        c.line_break();
        c.line_break();
        let before = c.backlog();
        c.line_break();
        assert_eq!(c.backlog(), before, "a break alone sends nothing");
        c.say("<< TWO");
        assert_eq!(screen(&drain(&mut c)), vec![">> ONE", "<< TWO"]);
    }

    #[test]
    fn a_clear_erases_and_the_next_text_sets_up_again() {
        let mut c = cc1();
        c.say("GONE");
        c.clear();
        c.say("BACK");
        let pairs = drain(&mut c);
        assert_eq!(screen(&pairs), vec!["BACK"]);
        let edm = [0x94, 0x2c];
        let at = pairs.iter().position(|p| *p == edm).expect("an EDM");
        assert_eq!(pairs[at + 1], edm, "sent twice");
        assert_eq!(pairs[at + 2], [0x94, 0x25], "roll-up again");
    }

    #[test]
    fn a_word_continued_takes_no_space() {
        let mut c = cc1();
        c.say("he go");
        c.append("es for");
        assert_eq!(screen(&drain(&mut c)), vec!["he goes for"]);
    }

    #[test]
    fn a_word_continued_past_the_row_is_cut_there() {
        // 608 has a backspace, but players draw it differently (ffmpeg
        // ignores it in roll-up, ccextractor leaves a letter behind), so a
        // live sender that continues a word past the row's end gets it cut.
        let mut c = cc1();
        c.append(">> welcome back to the grand fin");
        c.append("al");
        assert_eq!(
            screen(&drain(&mut c)),
            vec![">> welcome back to the grand fin", "al"]
        );
    }

    #[test]
    fn a_word_as_long_as_the_row_is_cut_where_it_must_be() {
        let mut c = cc1();
        c.append(&"Y".repeat(32));
        c.append("ZZ");
        assert_eq!(
            screen(&drain(&mut c)),
            vec!["Y".repeat(32), "ZZ".to_owned()]
        );
    }

    #[test]
    fn appended_text_keeps_its_spaces() {
        let mut c = cc1();
        c.append(">> GOOD");
        c.append(" CALL ");
        c.append("THERE");
        c.append("   ");
        c.append("NOW");
        c.append("S");
        assert_eq!(screen(&drain(&mut c)), vec![">> GOOD CALL THERE NOWS"]);
    }

    #[test]
    fn a_row_never_starts_with_a_space() {
        let mut c = cc1();
        c.append("  HELLO");
        c.line_break();
        c.append(" THERE");
        assert_eq!(screen(&drain(&mut c)), vec!["HELLO", "THERE"]);
    }

    #[test]
    fn an_odd_character_before_a_control_is_padded() {
        let mut c = cc1();
        c.say("A");
        c.line_break();
        c.say("B");
        let pairs = drain(&mut c);
        assert!(pairs.contains(&[0xc1, 0x80]), "{pairs:x?}");
    }

    #[test]
    fn an_extended_character_overwrites_its_stand_in() {
        let mut c = cc1();
        c.say("CAFÉ");
        let pairs = drain(&mut c);
        assert_eq!(screen(&pairs), vec!["CAFÉ"]);
    }

    #[test]
    fn cc3_is_field_two_and_cc2_is_channel_two() {
        let mut c = Captioner::new(Service::cc(3).unwrap(), 3);
        c.say("X");
        assert_eq!(drain(&mut c)[0], [0x15, 0x26], "RU3 on field 2");
        let mut c = Captioner::new(Service::cc(2).unwrap(), 2);
        c.say("X");
        assert_eq!(drain(&mut c)[0], [0x1c, 0x25], "RU2 on channel 2");
    }

    #[test]
    fn the_backlog_counts_pairs() {
        let mut c = cc1();
        c.say("ABC");
        // Six control pairs, then AB and C.
        assert_eq!(c.backlog(), 8);
        assert!((c.backlog_seconds() - 8.0 * 1001.0 / 30000.0).abs() < 1e-9);
    }
}
