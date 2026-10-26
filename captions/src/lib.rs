//! `captions`: CEA-608 captions put into an H.264 or HEVC stream's own access
//! units, as the ATSC A/53 SEI players read, so the captions travel with the
//! picture through every remux after it.
//!
//! Packets arrive on `v` and leave on `v`, one for one, in the order they
//! came and with their timestamps untouched; each leaves with one caption
//! SEI more than it had. The text comes two ways, either or both:
//!
//! - rows arguments (`cues`, `cues2`), the rows another module writes,
//!   paired with the packets by time so that each is applied when the
//!   stream reaches the time it carries;
//! - a loopback TCP socket (`port`), one JSON command a line, applied as it
//!   arrives. The socket is non-blocking and polled once a call, so a packet
//!   never waits on it.
//!
//! What the captions are, and how each frame's bytes are made, is
//! `cea608_core`'s. What is here is the node: its shape, the socket, the
//! frame rate, and the hold that gives the access units their pairs in
//! presentation order.

use std::io::{ErrorKind, Read};
use std::net::{TcpListener, TcpStream};
use std::time::Instant;

use cea608_core::captioner::Service;
use cea608_core::command::{lines, parse};
use cea608_core::cues::Row;
use cea608_core::order::Order;
use cea608_core::splice::{splice, Spliced};
use cea608_core::Captions;
use ffrwd_nal::config::{framing_of, Framing};
use ffrwd_node::{
    Bound, Format, Init, Input, Node, Out, Output, Packet, Rational, Result, Shape, StateRow, Tick,
};
use serde::{Deserialize, Serialize};

/// The codecs whose access units carry an A/53 SEI.
const CODECS: &[&str] = &["h264", "hevc"];

/// The rows arguments, in the order of their prefixes.
const TRACKS: [&str; 2] = ["cues", "cues2"];

const CUE_SCHEMA: &str = r#"{"type":"object","properties":{"text":{"type":"string"},"start_t":{"type":"number"},"end_t":{"type":"number"}},"required":["text","start_t","end_t"]}"#;

const PARAMS_SCHEMA: &str = r#"{
  "type": "object",
  "properties": {
    "port": {"type": "integer", "minimum": 1, "maximum": 65535, "description": "Loopback TCP port to take live commands on; none when left out."},
    "prefix": {"type": "string", "description": "Put at the start of each new utterance of `cues`, such as `>>`."},
    "prefix2": {"type": "string", "description": "The same for `cues2`."},
    "roll_up": {"type": "integer", "minimum": 2, "maximum": 4, "default": 2},
    "channel": {"type": "integer", "minimum": 1, "maximum": 4, "default": 1, "description": "CC1 to CC4."}
  },
  "additionalProperties": false
}"#;

/// How far past a packet's time rows are handed over early. Rows carry their
/// own times, and the earlier they are in hand the closer to those times
/// they go out.
const AHEAD_SECONDS: f64 = 2.0;

/// The deepest reorder H.264 and HEVC allow, in frames: the most the hold
/// can grow to, and so the latency the output declares.
const MAX_REORDER_FRAMES: u64 = 16;

#[derive(Deserialize)]
struct Params {
    port: Option<u16>,
    prefix: Option<String>,
    prefix2: Option<String>,
    roll_up: u8,
    channel: u8,
}

struct Inserter {
    v: u32,
    framing: Framing,
    time_base: Rational,
    /// Frames a second, once known: the call's, or worked out from the
    /// packets.
    rate: Option<(u32, u32)>,
    previous_pts: Option<i64>,
    captions: Captions,
    /// Each packet with the moment it arrived, for the hold it is given.
    order: Order<(Packet, Instant)>,
    #[cfg(feature = "trace")]
    traced: Vec<String>,
    #[cfg(feature = "trace")]
    last_call: Option<Instant>,
    socket: Option<Socket>,
    stats: Stats,
}

#[derive(Default, Serialize)]
struct Stats {
    event: &'static str,
    frames: u64,
    released: u64,
    captioned: u64,
    already_captioned: u64,
    failed: u64,
    bytes_added: u64,
    out_of_order: u64,
    reorder_depth: usize,
    rows: u64,
    commands: u64,
    edits_unsent: usize,
    pairs_unsent: usize,
    /// Wall time spent in `process`, in seconds.
    process_seconds: f64,
    /// The longest one `process` call took, in milliseconds.
    longest_call_ms: f64,
    /// How long packets stayed in the module, arrival to departure.
    hold_ms_mean: f64,
    hold_ms_median: f64,
    hold_ms_p99: f64,
    hold_ms_max: f64,
    /// The holds by the millisecond, the last bucket a second or more.
    #[serde(skip)]
    holds: Vec<u64>,
}

impl Stats {
    fn held(&mut self, ms: f64) {
        if self.holds.is_empty() {
            self.holds = vec![0; 1001];
        }
        self.holds[(ms as usize).min(1000)] += 1;
        self.hold_ms_max = self.hold_ms_max.max(ms);
        self.hold_ms_mean += ms;
        self.released += 1;
    }

    /// The hold `fraction` of the packets stayed no longer than, to the
    /// millisecond.
    fn hold_quantile(&self, fraction: f64) -> f64 {
        let wanted = (self.released as f64 * fraction).ceil() as u64;
        let mut seen = 0;
        for (ms, count) in self.holds.iter().enumerate() {
            seen += count;
            if seen >= wanted {
                return ms as f64;
            }
        }
        1000.0
    }
}

/// The live socket: one connection at a time, a new one replacing the old,
/// so a runner that reconnects is never locked out.
struct Socket {
    listener: TcpListener,
    conn: Option<TcpStream>,
    buffer: Vec<u8>,
}

impl Socket {
    fn bind(port: u16) -> Result<Socket> {
        let listener = TcpListener::bind(("127.0.0.1", port))
            .map_err(|e| format!("captions: bind 127.0.0.1:{port}: {e}"))?;
        listener
            .set_nonblocking(true)
            .map_err(|e| format!("captions: a non-blocking listener: {e}"))?;
        eprintln!("captions: taking caption commands on 127.0.0.1:{port}");
        Ok(Socket {
            listener,
            conn: None,
            buffer: Vec::new(),
        })
    }

    /// Every complete line that arrived since the last call.
    fn poll(&mut self) -> Vec<String> {
        loop {
            match self.listener.accept() {
                Ok((stream, _)) => {
                    if stream.set_nonblocking(true).is_err() {
                        eprintln!("captions: could not make a connection non-blocking");
                        continue;
                    }
                    if self.conn.is_some() {
                        eprintln!("captions: a new connection replaces the old one");
                    }
                    self.conn = Some(stream);
                    self.buffer.clear();
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                Err(e) => {
                    eprintln!("captions: accept: {e}");
                    break;
                }
            }
        }
        let Some(conn) = self.conn.as_mut() else {
            return Vec::new();
        };
        let mut chunk = [0u8; 16 * 1024];
        loop {
            match conn.read(&mut chunk) {
                Ok(0) => {
                    self.conn = None;
                    // A last line the runner did not end with a newline is
                    // still a line once the connection closes.
                    self.buffer.push(b'\n');
                    break;
                }
                Ok(n) => self.buffer.extend_from_slice(&chunk[..n]),
                Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                Err(e) => {
                    eprintln!("captions: read: {e}");
                    self.conn = None;
                    break;
                }
            }
        }
        lines(&mut self.buffer)
    }
}

fn latency(rate: Option<Rational>) -> f64 {
    rate.map_or(1.0, |rate| rate.duration(MAX_REORDER_FRAMES))
}

/// A rate as a positive fraction, reduced.
fn fraction(num: i64, den: i64) -> Option<(u32, u32)> {
    if num <= 0 || den <= 0 {
        return None;
    }
    let mut a = num;
    let mut b = den;
    while b != 0 {
        (a, b) = (b, a % b);
    }
    let (num, den) = (num / a, den / a);
    Some((u32::try_from(num).ok()?, u32::try_from(den).ok()?))
}

impl Inserter {
    /// The frame rate to pack the frame at `pts` with, learning it from the
    /// packets where the call did not say: the packet's duration, else the
    /// step from the frame before.
    fn rate_for(&mut self, duration: Option<i64>, pts: i64) -> (u32, u32) {
        if self.rate.is_none() {
            let tb = self.time_base;
            let step = duration
                .filter(|d| *d > 0)
                .or_else(|| self.previous_pts.map(|prev| pts - prev).filter(|d| *d > 0));
            if let Some(step) = step {
                self.rate = fraction(i64::from(tb.den), i64::from(tb.num) * step);
            }
        }
        self.previous_pts = Some(pts);
        self.rate.unwrap_or((30000, 1001))
    }

    /// Gives the next packets in presentation order their captions.
    fn settle(&mut self, closing: bool) {
        while let Some((seq, pts)) = self.order.settle(closing) {
            let duration = self.order.get_mut(seq).and_then(|(p, _)| p.duration);
            let (num, den) = self.rate_for(duration, pts);
            let t = self.time_base.seconds(pts);
            let payload = self.captions.frame(t, num, den);
            #[cfg(feature = "trace")]
            for command in self.traced.drain(..) {
                eprintln!("captions trace: {command} went into the frame at {t:.4} s");
            }
            let framing = self.framing;
            let Some((packet, _)) = self.order.get_mut(seq) else {
                continue;
            };
            self.stats.frames += 1;
            match splice(framing, &packet.data, &payload) {
                Ok(Spliced::Into(data)) => {
                    self.stats.bytes_added += (data.len() - packet.data.len()) as u64;
                    packet.data = data;
                    self.stats.captioned += 1;
                }
                Ok(Spliced::AlreadyCaptioned) => {
                    if self.stats.already_captioned == 0 {
                        eprintln!(
                            "captions: the encoder already put captions in the frame at {t:.3} s; \
                             frames that carry their own are left as they are"
                        );
                    }
                    self.stats.already_captioned += 1;
                }
                Err(reason) => {
                    if self.stats.failed == 0 {
                        eprintln!(
                            "captions: the frame at {t:.3} s goes out without captions: {reason}"
                        );
                    }
                    self.stats.failed += 1;
                }
            }
        }
    }
}

impl Node for Inserter {
    const NAME: &'static str = "captions";
    const VERSION: &'static str = env!("CARGO_PKG_VERSION");
    const PARAMS_SCHEMA: &'static str = PARAMS_SCHEMA;
    type Params = Params;

    fn shape(_params: &Params, bound: &Bound) -> Result<Shape> {
        let mut shape = Shape::new().input(Input::packets("v").clock().codecs(CODECS));
        for track in TRACKS {
            shape = shape.input(
                Input::rows(track)
                    .optional()
                    .interval()
                    .ahead(AHEAD_SECONDS)
                    .state()
                    .schema_json(CUE_SCHEMA),
            );
        }
        // No format of its own: the packets leave in the codec, time base
        // and extradata they came in.
        Ok(shape.output(Output::packets("v").latency(latency(bound.rate_of("v")))))
    }

    fn init(params: Params, init: &Init) -> Result<Inserter> {
        let v = init.stream("v")?;
        let Some(Format::Packets(coded)) = &v.format else {
            return Err("captions writes coded packets, and `v` is not a coded stream".into());
        };
        let framing = framing_of(&coded.codec, &coded.extradata).map_err(|_| {
            format!(
                "captions writes {} and not {}",
                CODECS.join(", "),
                coded.codec
            )
        })?;
        if matches!(framing, Framing::Av1) {
            return Err(format!("captions writes {} and not av1", CODECS.join(", ")).into());
        }
        let time_base = coded.time_base;
        if time_base.num <= 0 || time_base.den <= 0 {
            return Err(format!(
                "a time base of {}/{} is not a fraction of a second",
                time_base.num, time_base.den
            )
            .into());
        }
        let service = Service::cc(params.channel).ok_or("channel is 1 to 4")?;
        eprintln!(
            "captions: CC{}, {} rows of roll-up, into {} {}, reordered up to {} frames",
            params.channel,
            params.roll_up,
            coded.codec,
            match framing {
                Framing::AnnexB(_) => "with start codes",
                _ => "with length prefixes",
            },
            v.decode_delay
        );
        let socket = params.port.map(Socket::bind).transpose()?;
        let rate = v
            .hint
            .rate
            .and_then(|r| fraction(i64::from(r.num), i64::from(r.den)));
        Ok(Inserter {
            v: v.id,
            framing,
            time_base,
            rate,
            previous_pts: None,
            captions: Captions::new(service, params.roll_up, vec![params.prefix, params.prefix2]),
            order: Order::new(v.decode_delay as usize),
            #[cfg(feature = "trace")]
            traced: Vec::new(),
            #[cfg(feature = "trace")]
            last_call: None,
            socket,
            stats: Stats {
                event: "summary",
                ..Stats::default()
            },
        })
    }

    fn fold(&mut self, row: StateRow) -> Result<()> {
        let Some(track) = TRACKS.iter().position(|name| *name == row.port) else {
            return Ok(());
        };
        match row.row::<Row>() {
            Ok(cue) => {
                self.stats.rows += 1;
                self.captions.row(track, &cue);
            }
            // The rows are a transcript, and one that will not read is one
            // line of captions lost, not a reason to end the broadcast.
            Err(err) => eprintln!("captions: a row skipped: {err}"),
        }
        Ok(())
    }

    fn process(&mut self, tick: &Tick, out: &mut Out) -> Result<()> {
        let started = Instant::now();
        if let Some(socket) = self.socket.as_mut() {
            for line in socket.poll() {
                match parse(&line) {
                    Ok(commands) => {
                        for command in commands {
                            self.stats.commands += 1;
                            #[cfg(feature = "trace")]
                            self.traced.push(format!(
                                "{command:?} (arrived at unix {:.4})",
                                std::time::SystemTime::now()
                                    .duration_since(std::time::UNIX_EPOCH)
                                    .map_or(0.0, |d| d.as_secs_f64())
                            ));
                            self.captions.live(command);
                        }
                    }
                    Err(err) => eprintln!("captions: a line skipped: {err}: {line}"),
                }
            }
        }
        for packet in tick.packets(self.v) {
            let pts = packet.pts;
            self.order.push((packet, Instant::now()), pts);
        }
        let last = tick.last();
        self.settle(last);
        for (packet, arrived) in self.order.release() {
            let held = arrived.elapsed().as_secs_f64() * 1000.0;
            self.stats.held(held);
            out.packet("v", packet)?;
        }
        for note in self.captions.notes() {
            eprintln!("captions: {note}");
        }
        let took = started.elapsed();
        #[cfg(feature = "trace")]
        {
            let gap = self
                .last_call
                .map_or(0.0, |last| (started - last).as_secs_f64());
            if gap > 0.5 || took.as_secs_f64() > 0.05 {
                eprintln!(
                    "captions trace: a call {gap:.3} s after the last one took {:.1} ms, at the tick {:.3} s",
                    took.as_secs_f64() * 1000.0,
                    tick.seconds()
                );
            }
            self.last_call = Some(Instant::now());
        }
        self.stats.process_seconds += took.as_secs_f64();
        self.stats.longest_call_ms = self.stats.longest_call_ms.max(took.as_secs_f64() * 1000.0);
        if last {
            let (edits, pairs) = self.captions.unsent();
            self.stats.edits_unsent = edits;
            self.stats.pairs_unsent = pairs;
            self.stats.out_of_order = self.order.out_of_order();
            self.stats.reorder_depth = self.order.depth();
            if self.stats.released > 0 {
                self.stats.hold_ms_mean /= self.stats.released as f64;
                self.stats.hold_ms_median = self.stats.hold_quantile(0.5);
                self.stats.hold_ms_p99 = self.stats.hold_quantile(0.99);
            }
            if edits + pairs > 0 {
                eprintln!(
                    "captions: the stream ended with {edits} edits not yet due and {pairs} pairs not yet sent"
                );
            }
            if let Ok(line) = serde_json::to_string(&self.stats) {
                eprintln!("captions: {line}");
            }
            out.report(&self.stats)?;
        }
        Ok(())
    }
}

ffrwd_node::export!(Inserter);

#[cfg(test)]
mod tests {
    use super::*;
    use cea608_core::splice::cc_data_in;
    use ffrwd_nal::annexb::START_CODE;
    use ffrwd_node::mock::Harness;
    use ffrwd_node::{BoundStream, CodedFormat, CodedStream, CodedVideo, Kind, Payload, Runner};

    fn coded(port: &str, id: u32, extradata: Vec<u8>, rate: Option<Rational>) -> BoundStream {
        let time_base = Rational::new(1, 90000);
        let mut stream = BoundStream::new(port, id, time_base);
        stream.format = Some(Format::Packets(CodedStream {
            codec: "h264".into(),
            time_base,
            format: CodedFormat::Video(CodedVideo::default()),
            extradata,
            profile: None,
            level: None,
        }));
        stream.hint.rate = rate;
        stream
    }

    fn annexb_au(idr: bool) -> Vec<u8> {
        let slice: &[u8] = if idr {
            &[0x65, 0x88, 0x84, 0x00]
        } else {
            &[0x41, 0x9a, 0x01, 0x02]
        };
        [&START_CODE[..], slice].concat()
    }

    #[test]
    fn the_shape_is_packets_in_and_out_with_two_optional_rows_inputs() {
        let shape = Runner::<Inserter>::shape("{}", &["v".to_owned()]).expect("a shape");
        assert_eq!(shape.clock_input(), Some("v"));
        assert_eq!(shape.find_input("v").unwrap().kind, Kind::Packets);
        for track in TRACKS {
            let input = shape.find_input(track).expect("a rows input");
            assert_eq!(input.kind, Kind::Data);
            assert!(!input.required);
        }
        assert_eq!(shape.outputs[0].kind, Kind::Packets);
        let bound = Bound::new(&["v"]).rate("v", Rational::new(60, 1));
        let shape = Runner::<Inserter>::shape("{}", &bound).unwrap();
        assert!((shape.outputs[0].latency - 16.0 / 60.0).abs() < 1e-9);
    }

    #[test]
    fn params_out_of_range_are_refused() {
        for params in [
            r#"{"roll_up": 5}"#,
            r#"{"channel": 0}"#,
            r#"{"port": 70000}"#,
        ] {
            assert!(
                Runner::<Inserter>::shape(params, &["v".to_owned()]).is_err(),
                "{params}"
            );
        }
    }

    #[test]
    fn every_packet_leaves_once_with_its_captions_and_rows_land_on_time() {
        let mut h = Harness::<Inserter>::new(
            r#"{"prefix": ">>"}"#,
            vec![
                coded("v", 0, Vec::new(), Some(Rational::new(30, 1))),
                BoundStream::rows("cues", 1, Rational::new(1, 1000)),
            ],
        )
        .expect("a harness");
        let mut texts = Vec::new();
        let mut left = 0;
        for i in 0..90i64 {
            let mut tick = h.tick(i * 3000).packet(
                0,
                Packet {
                    pts: i * 3000,
                    dts: Some(i * 3000),
                    duration: Some(3000),
                    keyframe: i == 0,
                    data: annexb_au(i == 0),
                },
            );
            if i == 0 {
                tick = tick.message(
                    1,
                    1000,
                    br#"{"text":"GOOD CALL","start_t":1.0,"end_t":1.5,"_arg":"cues"}"#,
                );
            }
            if i == 89 {
                tick = tick.last();
            }
            let emitted = h.process(&tick).expect("a call");
            for payload in emitted.on("v") {
                let Payload::Packet(p) = payload else {
                    continue;
                };
                left += 1;
                let cc = cc_data_in(Framing::AnnexB(ffrwd_nal::Codec::H264), &p.data);
                assert_eq!(cc.len(), 1, "one caption SEI in every access unit");
                assert_eq!(cc[0][0] & 0x1f, 20, "cc_count at 30 fps");
                let first = &cc[0][2..5];
                if first[0] == 0xfc && first[1] & 0x7f >= 0x20 {
                    texts.push((p.pts, first[1] & 0x7f, first[2] & 0x7f));
                }
            }
        }
        assert_eq!(left, 90);
        let first_text = texts.first().expect("text went out");
        assert!(
            first_text.0 >= 90000,
            "nothing before the row's time: {first_text:?}"
        );
        let text: String = texts
            .iter()
            .flat_map(|(_, a, b)| [*a as char, *b as char])
            .filter(|c| *c != '\0')
            .collect();
        assert_eq!(text, ">> GOOD CALL");
    }
}
