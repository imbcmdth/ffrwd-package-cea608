//! One frame's caption SEI put into its access unit, before the first coded
//! slice, with every other NAL left as the encoder wrote it. ffrwd-nal does
//! the byte level; what is here is the A/53 message and the check that the
//! encoder did not already carry captions.

use ffrwd_nal::annexb::{scan_nals, split_length_prefixed};
use ffrwd_nal::config::Framing;
use ffrwd_nal::h26x::access_units;
use ffrwd_nal::sei::{
    insert_sei_annexb, insert_sei_length_prefixed, parse_sei, write_sei, SeiMessage,
};
use ffrwd_nal::Codec;

use crate::pack::{T35_HEADER, T35_PAYLOAD_TYPE};

/// What became of one access unit.
#[derive(Debug, PartialEq, Eq)]
pub enum Spliced {
    /// The access unit with the captions in it.
    Into(Vec<u8>),
    /// The encoder put A/53 captions in already; a second set would be two
    /// streams of pairs a decoder interleaves into nonsense.
    AlreadyCaptioned,
}

/// `payload` (an A/53 T.35 payload) put into `packet`, one access unit
/// framed as `framing` says.
pub fn splice(framing: Framing, packet: &[u8], payload: &[u8]) -> Result<Spliced, String> {
    let message = [SeiMessage {
        payload_type: T35_PAYLOAD_TYPE,
        payload: payload.to_vec(),
    }];
    match framing {
        Framing::AnnexB(codec) => {
            let nals = scan_nals(packet);
            if nals.iter().any(|nal| captions_in(nal.bytes, codec)) {
                return Ok(Spliced::AlreadyCaptioned);
            }
            let temporal_id_plus1 = access_units(packet, codec)
                .first()
                .map_or(1, |unit| unit.temporal_id_plus1);
            let sei = write_sei(&message, codec, temporal_id_plus1);
            insert_sei_annexb(packet, &sei, codec)
                .map(Spliced::Into)
                .map_err(|err| err.to_string())
        }
        Framing::LengthPrefixed { codec, length_size } => {
            let nals = split_length_prefixed(packet, length_size).map_err(|e| e.to_string())?;
            if nals.iter().any(|nal| captions_in(nal, codec)) {
                return Ok(Spliced::AlreadyCaptioned);
            }
            let temporal_id_plus1 = nals
                .iter()
                .find(|nal| codec.is_vcl(nal))
                .map_or(1, |nal| codec.temporal_id_plus1(nal));
            let sei = write_sei(&message, codec, temporal_id_plus1);
            insert_sei_length_prefixed(packet, &sei, length_size, codec)
                .map(Spliced::Into)
                .map_err(|err| err.to_string())
        }
        Framing::Av1 => Err("AV1 carries no A/53 SEI".to_owned()),
    }
}

/// Whether `nal` is an SEI carrying A/53 `cc_data`.
pub fn captions_in(nal: &[u8], codec: Codec) -> bool {
    if !codec.is_prefix_sei(nal) {
        return false;
    }
    parse_sei(nal, codec).is_ok_and(|messages| {
        messages
            .iter()
            .any(|m| m.payload_type == T35_PAYLOAD_TYPE && m.payload.starts_with(&T35_HEADER))
    })
}

/// Every A/53 `cc_data()` in one access unit, for tests and for reading a
/// stream back.
pub fn cc_data_in(framing: Framing, packet: &[u8]) -> Vec<Vec<u8>> {
    let (nals, codec): (Vec<&[u8]>, Codec) = match framing {
        Framing::AnnexB(codec) => (scan_nals(packet).iter().map(|n| n.bytes).collect(), codec),
        Framing::LengthPrefixed { codec, length_size } => (
            split_length_prefixed(packet, length_size).unwrap_or_default(),
            codec,
        ),
        Framing::Av1 => return Vec::new(),
    };
    nals.into_iter()
        .filter(|nal| codec.is_prefix_sei(nal))
        .filter_map(|nal| parse_sei(nal, codec).ok())
        .flatten()
        .filter(|m| m.payload_type == T35_PAYLOAD_TYPE && m.payload.starts_with(&T35_HEADER))
        .map(|m| m.payload[T35_HEADER.len()..m.payload.len().saturating_sub(1)].to_vec())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ffrwd_nal::annexb::{annexb_to_length_prefixed, split_nals, START_CODE};
    use ffrwd_nal::sei::USER_DATA_UNREGISTERED;

    const CC_DATA: [u8; 8] = [0xc2, 0xff, 0xfc, 0x94, 0x25, 0xf9, 0x80, 0x80];

    fn payload() -> Vec<u8> {
        crate::pack::t35_payload(&CC_DATA)
    }

    /// An x264-shaped access unit: delimiter, SPS, PPS, the encoder's own
    /// SEI, then two slices of one IDR picture.
    fn access_unit() -> Vec<Vec<u8>> {
        let settings = write_sei(
            &[SeiMessage {
                payload_type: USER_DATA_UNREGISTERED,
                payload: [&[0xdc_u8; 16][..], b"x264 - core 164"].concat(),
            }],
            Codec::H264,
            1,
        );
        vec![
            vec![0x09, 0xf0],
            vec![0x67, 0x64, 0x00, 0x1f, 0xac],
            vec![0x68, 0xeb, 0xec, 0xb2],
            settings,
            vec![0x65, 0x88, 0x84, 0x00, 0x21],
            vec![0x65, 0x08, 0x84, 0x00, 0x22],
        ]
    }

    fn annexb(nals: &[Vec<u8>]) -> Vec<u8> {
        nals.iter()
            .flat_map(|nal| START_CODE.iter().chain(nal.iter()).copied())
            .collect()
    }

    #[test]
    fn annex_b_takes_the_sei_before_the_first_slice_and_nothing_else_moves() {
        let nals = access_unit();
        let packet = annexb(&nals);
        let Spliced::Into(out) = splice(Framing::AnnexB(Codec::H264), &packet, &payload()).unwrap()
        else {
            panic!("not spliced");
        };
        let got = split_nals(&out);
        assert_eq!(got.len(), nals.len() + 1);
        for i in 0..4 {
            assert_eq!(got[i], &nals[i][..]);
        }
        assert!(captions_in(got[4], Codec::H264));
        assert_eq!(got[5], &nals[4][..]);
        assert_eq!(got[6], &nals[5][..]);
        assert_eq!(
            cc_data_in(Framing::AnnexB(Codec::H264), &out),
            vec![CC_DATA.to_vec()]
        );
    }

    #[test]
    fn length_prefixed_takes_it_the_same_way() {
        let nals = access_unit();
        let framing = Framing::LengthPrefixed {
            codec: Codec::H264,
            length_size: 4,
        };
        let packet = annexb_to_length_prefixed(&annexb(&nals), 4).unwrap();
        let Spliced::Into(out) = splice(framing, &packet, &payload()).unwrap() else {
            panic!("not spliced");
        };
        let got = split_length_prefixed(&out, 4).unwrap();
        assert_eq!(got.len(), nals.len() + 1);
        assert!(captions_in(got[4], Codec::H264));
        assert_eq!(got[3], &nals[3][..]);
        assert_eq!(got[5], &nals[4][..]);
        assert_eq!(cc_data_in(framing, &out), vec![CC_DATA.to_vec()]);
        assert_eq!(out.len(), packet.len() + 4 + got[4].len());
    }

    #[test]
    fn hevc_takes_a_prefix_sei_on_the_slices_layer() {
        // A one-slice HEVC picture on temporal layer 2.
        let slice = vec![0x02, 0x03, 0x80, 0x11];
        let packet = annexb(std::slice::from_ref(&slice));
        let Spliced::Into(out) = splice(Framing::AnnexB(Codec::H265), &packet, &payload()).unwrap()
        else {
            panic!("not spliced");
        };
        let got = split_nals(&out);
        assert_eq!(got[0][0] >> 1 & 0x3f, 39, "a prefix SEI");
        assert_eq!(got[0][1] & 0x07, 3);
        assert_eq!(got[1], &slice[..]);
    }

    #[test]
    fn an_access_unit_the_encoder_captioned_is_left_alone() {
        let packet = {
            let mut nals = access_unit();
            nals.insert(
                4,
                write_sei(
                    &[SeiMessage {
                        payload_type: T35_PAYLOAD_TYPE,
                        payload: payload(),
                    }],
                    Codec::H264,
                    1,
                ),
            );
            annexb(&nals)
        };
        assert_eq!(
            splice(Framing::AnnexB(Codec::H264), &packet, &payload()),
            Ok(Spliced::AlreadyCaptioned)
        );
    }

    #[test]
    fn a_payload_with_start_codes_in_it_is_escaped() {
        let cc = [0xc2, 0xff, 0xfc, 0x00, 0x00, 0xf9, 0x00, 0x01];
        let packet = annexb(&access_unit());
        let Spliced::Into(out) = splice(
            Framing::AnnexB(Codec::H264),
            &packet,
            &crate::pack::t35_payload(&cc),
        )
        .unwrap() else {
            panic!("not spliced");
        };
        assert_eq!(split_nals(&out).len(), access_unit().len() + 1);
        assert_eq!(
            cc_data_in(Framing::AnnexB(Codec::H264), &out),
            vec![cc.to_vec()]
        );
    }

    #[test]
    fn av1_is_refused() {
        assert!(splice(Framing::Av1, &[0x12, 0x00], &payload()).is_err());
    }
}
