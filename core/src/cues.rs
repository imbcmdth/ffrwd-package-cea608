//! Rows of speech as roll-up edits, each at the time it belongs on screen.
//!
//! Two kinds of producer write these rows. A whole-cue producer (whisper's
//! `transcribe`, a caption file) writes each utterance once, finished. A
//! streaming producer (parakeet's `transcribe`) writes the utterance so far
//! every time it grows, all the rows of one utterance sharing a `start_t`,
//! and then the same text once more with `final` true. Both come out the
//! same way: a new utterance starts a row of its own at its `start_t`, a
//! row that grows sends only the words it added, and a finished one stays
//! on screen until the next.
//!
//! A streaming row can end inside a word, since its producer writes a piece
//! of a word at a time ("the grand fin", then "the grand final"). The word a
//! row ends with is held until a later row shows it finished, by a space
//! after it or a `final`, so no word goes out in two pieces. Each word goes
//! out at the `end_t` of the first row that held it whole, which with the
//! rows in hand ahead of the picture is when it was said.

use std::cmp::Ordering;
use std::collections::BinaryHeap;

use serde::Deserialize;

use crate::captioner::{Captioner, PAIRS_PER_SECOND, ROW_START_PAIRS};

/// The most characters of a voice's name a row shows. A row is 32 columns,
/// and the marker, its space and the colon take up to five of them.
pub const MAX_NAME_CHARS: usize = 12;

/// How long a new row's control codes take to send.
const ROW_START_SECONDS: f64 = ROW_START_PAIRS as f64 / PAIRS_PER_SECOND;

/// One row as a rows argument hands it over. `final` is a streaming
/// producer's; a row without it is a whole cue, finished as it stands.
#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct Row {
    pub text: String,
    pub start_t: f64,
    #[serde(default)]
    pub end_t: Option<f64>,
    #[serde(default, rename = "final")]
    pub finished: Option<bool>,
    #[serde(default)]
    pub voice: Option<String>,
}

/// What a row does to the captions.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Edit {
    /// A new utterance: a row of its own with these words.
    Utterance(String),
    /// More words of the utterance already on screen.
    Grow(String),
}

/// One producer's rows, followed from one row to the next.
#[derive(Debug, Default)]
pub struct Track {
    /// The open utterance's rows: each one's text, spaces collapsed, and
    /// its `end_t`.
    rows: Vec<(String, f64)>,
    start_t: f64,
    /// Whether that utterance may still grow.
    open: bool,
    /// How much of its text has been sent, in bytes, always at the end of a
    /// word.
    sent: usize,
}

impl Track {
    /// The edits `row` makes, each with its time.
    pub fn row(&mut self, row: &Row) -> Vec<(f64, Edit)> {
        let text = row.text.split_whitespace().collect::<Vec<_>>().join(" ");
        let end_t = row.end_t.unwrap_or(row.start_t).max(row.start_t);
        let finished = row.finished.unwrap_or(true);
        let continues = self.open
            && same_time(row.start_t, self.start_t)
            && self
                .rows
                .last()
                .is_some_and(|(last, _)| text.starts_with(&last[..self.sent]));
        if !continues {
            // A new utterance, or a streaming producer that rewrote words
            // already sent: the screen cannot take those back, so the
            // utterance starts again on a row of its own.
            self.rows.clear();
            self.sent = 0;
            self.start_t = row.start_t;
        }
        self.rows.push((text.clone(), end_t));
        self.open = !finished;
        let whole = if finished {
            text.len()
        } else {
            text.rfind(' ').unwrap_or(0)
        };
        if whole <= self.sent {
            return Vec::new();
        }
        let mut edits: Vec<(f64, Edit)> = Vec::new();
        let mut at = self.sent;
        for word in text[self.sent..whole].split(' ').filter(|w| !w.is_empty()) {
            let begin = at + text[at..].find(word).unwrap_or(0);
            at = begin + word.len();
            let first = self.sent == 0 && edits.is_empty();
            // A whole cue goes up at once, at its start.
            let t = if first || (finished && self.rows.len() == 1) {
                self.start_t
            } else {
                self.said(&text, at)
            };
            match edits.last_mut() {
                Some((last_t, Edit::Utterance(words) | Edit::Grow(words))) if *last_t == t => {
                    words.push(' ');
                    words.push_str(word);
                }
                _ if first => edits.push((t, Edit::Utterance(word.to_owned()))),
                _ => edits.push((t, Edit::Grow(word.to_owned()))),
            }
        }
        self.sent = whole;
        edits
    }

    /// When the word ending at `text[..end]` was said, from the first row
    /// that held it whole: that row's `end_t` when it is the row's last
    /// word, whose time `end_t` is, and otherwise the row before's, since it
    /// was said after the words that row ended with and before this row's
    /// last.
    fn said(&self, text: &str, end: usize) -> f64 {
        let prefix = &text[..end];
        let first = self.rows.iter().position(|(row, _)| {
            row.starts_with(prefix) && matches!(row.as_bytes().get(end), None | Some(b' '))
        });
        match first {
            Some(at) if self.rows[at].0.len() == end => self.rows[at].1,
            Some(0) | None => self.start_t,
            Some(at) => self.rows[at - 1].1,
        }
    }
}

fn same_time(a: f64, b: f64) -> bool {
    (a - b).abs() < 1e-6
}

/// An edit waiting for its time.
#[derive(Clone, Debug)]
pub struct Timed {
    pub at: f64,
    pub track: usize,
    pub edit: Edit,
    voice: Option<String>,
    seq: u64,
}

impl PartialEq for Timed {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for Timed {}

impl PartialOrd for Timed {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Timed {
    /// Reversed, so the heap hands out the earliest first, and two edits at
    /// one time in the order they were made.
    fn cmp(&self, other: &Self) -> Ordering {
        other
            .at
            .total_cmp(&self.at)
            .then_with(|| other.seq.cmp(&self.seq))
    }
}

/// Every rows argument's edits, in the order of their times.
#[derive(Debug, Default)]
pub struct Schedule {
    tracks: Vec<(Track, Option<String>, Option<String>)>,
    waiting: BinaryHeap<Timed>,
    seq: u64,
}

impl Schedule {
    /// A schedule over one track per rows argument, each with the marker
    /// its new rows start with.
    pub fn new(prefixes: Vec<Option<String>>) -> Schedule {
        Schedule {
            tracks: prefixes
                .into_iter()
                .map(|prefix| (Track::default(), prefix, None))
                .collect(),
            ..Schedule::default()
        }
    }

    /// One row of track `track`.
    pub fn row(&mut self, track: usize, row: &Row) {
        let Some((follow, _, _)) = self.tracks.get_mut(track) else {
            return;
        };
        let voice = row.voice.as_deref().and_then(name_of);
        for (at, edit) in follow.row(row) {
            // A new row takes its roll-up, carriage return and preamble
            // before its first letter, so it starts that much early and the
            // letter lands on the row's time.
            let at = match edit {
                Edit::Utterance(_) => at - ROW_START_SECONDS,
                Edit::Grow(_) => at,
            };
            self.seq += 1;
            self.waiting.push(Timed {
                at,
                track,
                edit,
                voice: voice.clone(),
                seq: self.seq,
            });
        }
    }

    /// How many edits are waiting.
    pub fn len(&self) -> usize {
        self.waiting.len()
    }

    pub fn is_empty(&self) -> bool {
        self.waiting.is_empty()
    }

    /// Applies every edit due by `t` seconds.
    pub fn apply_due(&mut self, t: f64, captioner: &mut Captioner) {
        while self.waiting.peek().is_some_and(|next| next.at <= t) {
            let Some(timed) = self.waiting.pop() else {
                break;
            };
            let (_, prefix, last) = &mut self.tracks[timed.track];
            apply(captioner, timed.track, prefix.as_deref(), last, timed);
        }
    }
}

/// A voice as 608 shows it: capitals, spaces collapsed, at most
/// [`MAX_NAME_CHARS`] characters. A voice with nothing in it has no name.
fn name_of(voice: &str) -> Option<String> {
    let name: String = voice
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_uppercase()
        .chars()
        .take(MAX_NAME_CHARS)
        .collect();
    let name = name.trim_end().to_owned();
    (!name.is_empty()).then_some(name)
}

fn apply(
    captioner: &mut Captioner,
    track: usize,
    prefix: Option<&str>,
    last: &mut Option<String>,
    timed: Timed,
) {
    let Timed { edit, voice, .. } = timed;
    let words = match edit {
        Edit::Grow(words) if captioner.owner() == Some(Some(track)) => {
            captioner.say(&words);
            *last = voice;
            return;
        }
        // A new utterance, or more of one whose row is someone else's by now
        // or was cleared: the words start a row of their own.
        Edit::Utterance(words) | Edit::Grow(words) => words,
    };
    let speaker_changed = voice.is_some() && (*last != voice || captioner.owner().is_none());
    captioner.start_row_for(Some(track));
    let mut row = String::new();
    // With voices the marker means a change of speaker, as it does in live
    // captions: the same speaker's next utterance starts its row bare.
    let marked = voice.is_none() || speaker_changed;
    if let (true, Some(prefix)) = (marked, prefix) {
        row.push_str(prefix);
        row.push(' ');
    }
    if let (true, Some(name)) = (speaker_changed, &voice) {
        row.push_str(name);
        row.push_str(": ");
    }
    row.push_str(&words);
    captioner.say(&row);
    *last = voice;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::captioner::Service;

    fn row(text: &str, start_t: f64, end_t: f64, finished: Option<bool>) -> Row {
        Row {
            text: text.into(),
            start_t,
            end_t: Some(end_t),
            finished,
            voice: None,
        }
    }

    fn voiced(voice: &str, text: &str, start_t: f64, end_t: f64, finished: bool) -> Row {
        Row {
            voice: Some(voice.into()),
            ..row(text, start_t, end_t, Some(finished))
        }
    }

    fn grow(t: f64, words: &str) -> (f64, Edit) {
        (t, Edit::Grow(words.into()))
    }

    /// The text a schedule's edits put on screen, `|` for each new row.
    fn screen(s: &mut Schedule, rows: u8) -> String {
        let mut c = Captioner::new(Service::cc(1).unwrap(), rows);
        s.apply_due(f64::MAX, &mut c);
        let mut text = String::new();
        let mut state = cea608_types::Cea608State::default();
        while let Some(pair) = c.next_pair() {
            match state.decode(pair).unwrap() {
                Some(cea608_types::Cea608::Text(t)) => {
                    text.extend(t.char1);
                    text.extend(t.char2);
                }
                Some(cea608_types::Cea608::CarriageReturn(_)) => text.push('|'),
                _ => {}
            }
        }
        text
    }

    #[test]
    fn a_streaming_utterance_sends_each_word_once_when_it_is_whole() {
        let mut t = Track::default();
        let edits: Vec<_> = [
            row("he goes", 0.32, 0.4, Some(false)),
            row("he goes for the", 0.32, 0.72, Some(false)),
            row("he goes for the flank", 0.32, 1.12, Some(false)),
            row("he goes for the flank", 0.32, 1.12, Some(true)),
            row("that", 2.24, 2.24, Some(false)),
        ]
        .iter()
        .map(|r| t.row(r))
        .collect();
        assert_eq!(
            edits,
            vec![
                vec![(0.32, Edit::Utterance("he".into()))],
                // "goes" ended the row before, at 0.4, and "for" came after.
                vec![grow(0.4, "goes for")],
                vec![grow(0.72, "the")],
                vec![grow(1.12, "flank")],
                vec![],
            ]
        );
    }

    #[test]
    fn a_word_written_in_pieces_goes_out_whole() {
        let mut t = Track::default();
        t.row(&row("welcome back to the grand", 10.24, 11.2, Some(false)));
        assert_eq!(
            t.row(&row(
                "welcome back to the grand fin",
                10.24,
                11.36,
                Some(false)
            )),
            vec![grow(11.2, "grand")]
        );
        assert_eq!(
            t.row(&row(
                "welcome back to the grand final",
                10.24,
                11.6,
                Some(false)
            )),
            vec![]
        );
        assert_eq!(
            t.row(&row(
                "welcome back to the grand final",
                10.24,
                11.6,
                Some(true)
            )),
            vec![grow(11.6, "final")]
        );
    }

    #[test]
    fn whole_cues_are_each_an_utterance_even_when_one_repeats_another() {
        let mut t = Track::default();
        assert_eq!(
            t.row(&row("HELLO THERE", 1.0, 2.0, None)),
            vec![(1.0, Edit::Utterance("HELLO THERE".into()))]
        );
        assert_eq!(
            t.row(&row("HELLO THERE AGAIN", 2.5, 3.0, None)),
            vec![(2.5, Edit::Utterance("HELLO THERE AGAIN".into()))]
        );
        assert_eq!(
            t.row(&row("HELLO THERE AGAIN", 2.5, 3.0, None)),
            vec![(2.5, Edit::Utterance("HELLO THERE AGAIN".into()))],
            "a finished cue does not grow, even into itself"
        );
    }

    #[test]
    fn final_rows_alone_are_whole_cues() {
        let mut t = Track::default();
        assert_eq!(
            t.row(&row("one", 1.0, 1.5, Some(true))),
            vec![(1.0, Edit::Utterance("one".into()))]
        );
        assert_eq!(
            t.row(&row("one two", 1.0, 2.0, Some(true))),
            vec![(1.0, Edit::Utterance("one two".into()))]
        );
    }

    #[test]
    fn a_rewritten_utterance_starts_again() {
        let mut t = Track::default();
        t.row(&row("the red team", 1.0, 1.2, Some(false)));
        assert_eq!(
            t.row(&row("the rest of it", 1.0, 1.3, Some(false))),
            vec![(1.0, Edit::Utterance("the rest of".into()))]
        );
    }

    #[test]
    fn two_speakers_take_rows_of_their_own_with_their_markers() {
        let mut s = Schedule::new(vec![Some(">>".into()), Some("<<".into())]);
        s.row(0, &row("left one", 0.0, 0.2, Some(true)));
        s.row(1, &row("right", 0.5, 0.5, Some(true)));
        s.row(0, &row("left one two three", 0.8, 1.0, Some(true)));
        let mut c = Captioner::new(Service::cc(1).unwrap(), 3);
        s.apply_due(0.5 - ROW_START_SECONDS - 0.01, &mut c);
        assert_eq!(s.len(), 2);
        assert_eq!(screen(&mut s, 3), "|<< right|>> left one two three");
    }

    #[test]
    fn a_growing_utterance_whose_row_was_taken_goes_on_under_its_marker() {
        let mut s = Schedule::new(vec![Some(">>".into()), Some("<<".into())]);
        s.row(0, &row("we go now", 0.0, 0.3, Some(false)));
        s.row(1, &row("yes", 0.6, 0.6, Some(true)));
        s.row(0, &row("we go now and win", 0.0, 0.9, Some(true)));
        assert_eq!(screen(&mut s, 2), "|>> we go now and|<< yes|>> win");
    }

    #[test]
    fn a_change_of_voice_starts_a_row_that_names_the_speaker() {
        let mut s = Schedule::new(vec![Some(">>".into())]);
        s.row(0, &voiced("Mango", "ONE MORE", 0.0, 0.5, true));
        s.row(0, &voiced("Rex", "GO ON", 2.0, 2.5, true));
        assert_eq!(screen(&mut s, 3), "|>> MANGO: ONE MORE|>> REX: GO ON");
    }

    #[test]
    fn a_voice_that_continues_does_not_repeat_its_name() {
        let mut s = Schedule::new(vec![Some(">>".into())]);
        s.row(0, &voiced("Mango", "ONE MORE", 0.0, 0.5, true));
        s.row(0, &voiced("Mango", "AND AGAIN", 2.0, 2.5, true));
        assert_eq!(screen(&mut s, 3), "|>> MANGO: ONE MORE|AND AGAIN");
    }

    #[test]
    fn a_cue_without_a_voice_is_as_before() {
        let mut s = Schedule::new(vec![Some(">>".into())]);
        s.row(0, &row("ONE MORE", 0.0, 0.5, None));
        s.row(0, &row("AND AGAIN", 2.0, 2.5, None));
        assert_eq!(screen(&mut s, 3), "|>> ONE MORE|>> AND AGAIN");
        let mut s = Schedule::new(vec![None]);
        s.row(0, &voiced("Mango", "HI", 0.0, 0.5, true));
        assert_eq!(screen(&mut s, 2), "|MANGO: HI");
    }

    #[test]
    fn a_voice_after_a_cue_without_one_is_named_again() {
        let mut s = Schedule::new(vec![Some(">>".into())]);
        s.row(0, &voiced("Mango", "A", 0.0, 0.5, true));
        s.row(0, &row("B", 2.0, 2.5, None));
        s.row(0, &voiced("Mango", "C", 4.0, 4.5, true));
        assert_eq!(screen(&mut s, 4), "|>> MANGO: A|>> B|>> MANGO: C");
    }

    #[test]
    fn a_growing_utterance_resent_with_its_voice_names_it_once() {
        let mut s = Schedule::new(vec![Some(">>".into())]);
        s.row(0, &voiced("Mango", "WE GO", 0.0, 0.3, false));
        s.row(0, &voiced("Mango", "WE GO NOW", 0.0, 0.6, false));
        s.row(0, &voiced("Mango", "WE GO NOW", 0.0, 0.6, true));
        assert_eq!(screen(&mut s, 2), "|>> MANGO: WE GO NOW");
    }

    #[test]
    fn a_voice_is_tracked_for_each_rows_argument() {
        let mut s = Schedule::new(vec![Some(">>".into()), Some("<<".into())]);
        s.row(0, &voiced("Mango", "LEFT", 0.0, 0.2, true));
        s.row(1, &voiced("Mango", "RIGHT", 1.0, 1.2, true));
        assert_eq!(screen(&mut s, 3), "|>> MANGO: LEFT|<< MANGO: RIGHT");
    }

    #[test]
    fn a_long_name_is_clipped() {
        assert_eq!(
            name_of("Bartholomew the Third").as_deref(),
            Some("BARTHOLOMEW")
        );
        assert_eq!(
            name_of("Abcdefghijkl-mnop").as_deref(),
            Some("ABCDEFGHIJKL")
        );
        assert_eq!(name_of("  mango   rex "), Some("MANGO REX".into()));
        assert_eq!(name_of("   "), None);
        let mut s = Schedule::new(vec![Some(">>".into())]);
        s.row(0, &voiced("Bartholomew the Third", "HI", 0.0, 0.5, true));
        assert_eq!(screen(&mut s, 2), "|>> BARTHOLOMEW: HI");
    }
}
