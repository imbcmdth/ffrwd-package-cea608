//! A frame's caption bytes: the 608 pairs its fields carry, as ATSC A/53
//! `cc_data`, in the ITU-T T.35 payload an SEI message carries.
//!
//! How much a frame carries follows its rate. `cc_count` is the 9600 bit/s
//! of caption data spread over the frames, 20 at 30 fps and 10 at 60, and
//! line 21 is a field pair 30000/1001 times a second, so at 30 fps each
//! frame carries a field 1 and a field 2 pair, and at 60 fps the frames take
//! turns, one field each. cea708-types' writer does that arithmetic and the
//! padding; this module feeds it and wraps what it writes.

use cea708_types::{CCDataWriter, Cea608, Framerate};

use crate::captioner::{Captioner, Service};

/// The ITU-T T.35 header of an ATSC caption payload: the United States,
/// ATSC's provider code, `GA94`, and `cc_data`'s user data type.
pub const T35_HEADER: [u8; 8] = [0xb5, 0x00, 0x31, b'G', b'A', b'9', b'4', 0x03];

/// `user_data_registered_itu_t_t35`, the SEI payload type captions ride.
pub const T35_PAYLOAD_TYPE: u32 = 4;

/// A frame rate, as close to a broadcast rate as it is.
///
/// A rate within half a percent of one of the broadcast rates is taken as
/// that rate, so a 60 fps stream whose timestamps wobble, or whose rate was
/// worked out from them, still packs as 60.
pub fn snap(num: u32, den: u32) -> (u32, u32) {
    const RATES: [(u32, u32); 8] = [
        (24000, 1001),
        (24, 1),
        (25, 1),
        (30000, 1001),
        (30, 1),
        (50, 1),
        (60000, 1001),
        (60, 1),
    ];
    if num == 0 || den == 0 {
        return (30000, 1001);
    }
    let fps = f64::from(num) / f64::from(den);
    RATES
        .iter()
        .copied()
        .min_by(|a, b| {
            let da = (f64::from(a.0) / f64::from(a.1) - fps).abs();
            let db = (f64::from(b.0) / f64::from(b.1) - fps).abs();
            da.total_cmp(&db)
        })
        .filter(|(n, d)| ((f64::from(*n) / f64::from(*d)) - fps).abs() / fps < 0.005)
        .unwrap_or((num, den))
}

/// The pairs one field takes a frame at this rate: what is kept queued in
/// the writer so a frame never goes out short while text is waiting.
fn pairs_per_frame(num: u32, den: u32) -> usize {
    ((60.0 * f64::from(den) / f64::from(num)).round() as usize).max(1)
}

/// Builds each frame's `cc_data` from the captioner's queue.
pub struct Packer {
    writer: CCDataWriter,
    service: Service,
    /// How many pairs are in the writer, which it does not say exactly.
    queued: usize,
    /// Whether the writer's next field 1 slot comes before a field 2 one.
    field_one_next: bool,
}

impl Packer {
    pub fn new(service: Service) -> Packer {
        let mut writer = CCDataWriter::default();
        // Every frame carries its full cc_count: 608 slots that have
        // nothing to say are null pairs marked not valid, and the rest is
        // DTVCC padding. A decoder that counts on the frame's allotment, and
        // a remuxer that does, both see a steady stream.
        writer.set_output_cea608_padding(true);
        writer.set_output_padding(true);
        Packer {
            writer,
            service,
            queued: 0,
            field_one_next: true,
        }
    }

    /// One frame's `cc_data()` at `num`/`den` frames a second, topped up
    /// from `captioner` first.
    pub fn cc_data(&mut self, captioner: &mut Captioner, num: u32, den: u32) -> Vec<u8> {
        let (num, den) = snap(num, den);
        let per_frame = pairs_per_frame(num, den);
        while self.queued < per_frame {
            let Some([a, b]) = captioner.next_pair() else {
                break;
            };
            self.writer
                .push_cea608(if self.service.field == cea608_types::tables::Field::ONE {
                    Cea608::Field1(a, b)
                } else {
                    Cea608::Field2(a, b)
                });
            self.queued += 1;
        }
        let mut out = Vec::with_capacity(2 + 3 * 20);
        self.writer
            .write(Framerate::new(num, den), &mut out)
            .expect("a Vec takes every write");
        self.queued -= self.taken(per_frame).min(self.queued);
        out
    }

    /// How many of this service's pairs the frame just written took: the
    /// writer alternates field 1 and field 2 slots, `per_frame` a frame.
    fn taken(&mut self, per_frame: usize) -> usize {
        let mut ours = 0;
        for _ in 0..per_frame {
            let field_one = self.field_one_next;
            self.field_one_next = !self.field_one_next;
            if field_one == (self.service.field == cea608_types::tables::Field::ONE) {
                ours += 1;
            }
        }
        ours
    }

    /// Pairs handed to the writer and not yet sent.
    pub fn queued(&self) -> usize {
        self.queued
    }
}

/// The SEI payload for one frame's `cc_data`: the T.35 header, the data,
/// and the marker byte A/53 closes `cc_data()` with.
pub fn t35_payload(cc_data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(T35_HEADER.len() + cc_data.len() + 1);
    out.extend_from_slice(&T35_HEADER);
    out.extend_from_slice(cc_data);
    out.push(0xff);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use cea708_types::CCDataParser;

    fn cc1() -> Service {
        Service::cc(1).unwrap()
    }

    /// The (cc_valid, cc_type, byte, byte) triples of one cc_data().
    fn triples(cc_data: &[u8]) -> Vec<(bool, u8, u8, u8)> {
        let count = usize::from(cc_data[0] & 0x1f);
        assert_eq!(cc_data[0] & 0xe0, 0xc0, "reserved bit and process flag");
        assert_eq!(cc_data[1], 0xff, "reserved byte");
        assert_eq!(cc_data.len(), 2 + 3 * count);
        cc_data[2..]
            .chunks(3)
            .map(|t| {
                assert_eq!(t[0] & 0xf8, 0xf8, "marker and reserved bits");
                (t[0] & 0x04 != 0, t[0] & 0x03, t[1], t[2])
            })
            .collect()
    }

    #[test]
    fn rates_snap_to_the_broadcast_rate_they_are_near() {
        assert_eq!(snap(60, 1), (60, 1));
        assert_eq!(snap(60000, 1001), (60000, 1001));
        assert_eq!(snap(5994, 100), (60000, 1001));
        assert_eq!(snap(2997, 100), (30000, 1001));
        assert_eq!(snap(599, 10), (60000, 1001));
        assert_eq!(snap(1000, 1), (1000, 1), "nothing near it");
    }

    #[test]
    fn thirty_fps_carries_a_field_one_and_a_field_two_pair_in_twenty() {
        for (num, den) in [(30, 1), (30000, 1001)] {
            let mut c = Captioner::new(cc1(), 2);
            c.say("HELLO");
            let mut p = Packer::new(cc1());
            let t = triples(&p.cc_data(&mut c, num, den));
            assert_eq!(t.len(), 20, "{num}/{den}");
            assert_eq!(t[0], (true, 0, 0x94, 0x25), "RU2 in field 1");
            assert_eq!(t[1], (false, 1, 0x80, 0x80), "field 2 null, not valid");
            assert!(
                t[2..].iter().all(|x| *x == (false, 2, 0, 0)),
                "the rest DTVCC padding"
            );
        }
    }

    #[test]
    fn sixty_fps_carries_field_one_on_every_other_frame_in_ten() {
        for (num, den) in [(60, 1), (60000, 1001)] {
            let mut c = Captioner::new(cc1(), 2);
            c.say("HELLO");
            let mut p = Packer::new(cc1());
            let mut field1 = Vec::new();
            for frame in 0..8 {
                let t = triples(&p.cc_data(&mut c, num, den));
                assert_eq!(t.len(), 10, "{num}/{den}");
                let wanted_type = if frame % 2 == 0 { 0 } else { 1 };
                assert_eq!(t[0].1, wanted_type, "frame {frame}: fields take turns");
                if t[0].1 == 0 && t[0].0 {
                    field1.push([t[0].2, t[0].3]);
                }
            }
            // Four field 1 slots in eight frames: RU2 twice, CR twice.
            assert_eq!(
                field1,
                vec![[0x94, 0x25], [0x94, 0x25], [0x94, 0xad], [0x94, 0xad]]
            );
        }
    }

    #[test]
    fn idle_frames_are_padding_marked_not_valid() {
        let mut c = Captioner::new(cc1(), 2);
        let mut p = Packer::new(cc1());
        let t = triples(&p.cc_data(&mut c, 30000, 1001));
        assert_eq!(t[0], (false, 0, 0x80, 0x80));
        assert_eq!(t[1], (false, 1, 0x80, 0x80));
    }

    #[test]
    fn every_pair_comes_out_once_and_in_order_at_both_rates() {
        for (num, den) in [(30, 1), (60000, 1001), (25, 1), (24000, 1001)] {
            let mut c = Captioner::new(cc1(), 2);
            c.say("THE QUICK BROWN FOX JUMPS OVER THE LAZY DOG AGAIN AND AGAIN");
            let mut reference = Captioner::new(cc1(), 2);
            reference.say("THE QUICK BROWN FOX JUMPS OVER THE LAZY DOG AGAIN AND AGAIN");
            let want: Vec<[u8; 2]> = std::iter::from_fn(|| reference.next_pair()).collect();
            let mut p = Packer::new(cc1());
            let mut got = Vec::new();
            for _ in 0..200 {
                for t in triples(&p.cc_data(&mut c, num, den)) {
                    if t.0 && t.1 == 0 {
                        got.push([t.2, t.3]);
                    }
                }
            }
            assert_eq!(got, want, "{num}/{den}");
            assert_eq!(p.queued(), 0);
        }
    }

    #[test]
    fn cc3_goes_in_field_two() {
        let cc3 = Service::cc(3).unwrap();
        let mut c = Captioner::new(cc3, 2);
        c.say("HI");
        let mut p = Packer::new(cc3);
        let t = triples(&p.cc_data(&mut c, 30, 1));
        assert_eq!(
            t[0],
            (true, 0, 0x80, 0x80),
            "a valid null field 1 before it"
        );
        assert_eq!(t[1], (true, 1, 0x15, 0x25));
    }

    #[test]
    fn the_other_broadcast_rates_carry_what_the_writer_gives_them() {
        // (rate, cc_count, 608 slots a frame, field 1 pairs in 60 frames)
        for (num, den, count, slots, field1) in [
            (50, 1, 12, 1, 30),
            (25, 1, 24, 2, 60),
            (24, 1, 25, 3, 90),
            (24000, 1001, 25, 3, 90),
        ] {
            let mut c = Captioner::new(cc1(), 2);
            c.say(&"X".repeat(400));
            let mut p = Packer::new(cc1());
            let mut sent = 0;
            for _ in 0..60 {
                let t = triples(&p.cc_data(&mut c, num, den));
                assert_eq!(t.len(), count, "{num}/{den}");
                assert!(t[..slots].iter().all(|x| x.1 < 2), "{num}/{den}: 608 first");
                assert!(
                    t[slots..].iter().all(|x| x.1 >= 2),
                    "{num}/{den}: then DTVCC"
                );
                sent += t.iter().filter(|x| x.0 && x.1 == 0).count();
            }
            assert_eq!(sent, field1, "{num}/{den}");
        }
    }

    #[test]
    fn the_payload_parses_as_cc_data() {
        let mut c = Captioner::new(cc1(), 2);
        c.say("HI");
        let mut p = Packer::new(cc1());
        let payload = t35_payload(&p.cc_data(&mut c, 30, 1));
        assert_eq!(&payload[..8], &T35_HEADER);
        assert_eq!(*payload.last().unwrap(), 0xff);
        let mut parser = CCDataParser::new();
        parser.handle_cea608();
        parser
            .push(&payload[8..payload.len() - 1])
            .expect("cc_data");
        assert_eq!(
            parser.cea608().map(|c| c.to_vec()),
            Some(vec![cea708_types::Cea608::Field1(0x94, 0x25)])
        );
    }
}
