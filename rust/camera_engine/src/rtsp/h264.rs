use crate::media::EncodedFrame;

pub const NAL_IDR: u8 = 5;
pub const NAL_SPS: u8 = 7;
pub const NAL_PPS: u8 = 8;
pub const NAL_AUD: u8 = 9;

pub fn nal_type(nal: &[u8]) -> u8 {
    nal[0] & 0x1F
}

#[derive(Clone, Debug, PartialEq)]
pub struct ParameterSets {
    pub sps: Vec<u8>,
    pub pps: Vec<u8>,
}

// One encoded picture, split into NAL units without start codes
pub struct AccessUnit {
    pub nals: Vec<Vec<u8>>,
    // 90kHz RTP clock
    pub timestamp_90k: u32,
    pub keyframe: bool,
}

// Splits an Annex B buffer on its 3 or 4 byte start codes
pub fn split_annex_b(data: &[u8]) -> Vec<&[u8]> {
    let mut nals = Vec::new();
    let mut start: Option<usize> = None;
    let mut i = 0;

    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            if let Some(s) = start {
                nals.push(&data[s..i]);
            }
            i += 3;
            start = Some(i);
        } else {
            i += 1;
        }
    }
    if let Some(s) = start {
        nals.push(&data[s..]);
    }

    // A NAL unit never ends with a zero byte, those belong to the next 4 byte start code
    nals.into_iter()
        .map(|nal| {
            let end = nal.iter().rposition(|&b| b != 0).map_or(0, |p| p + 1);
            &nal[..end]
        })
        .filter(|nal| !nal.is_empty())
        .collect()
}

// Turns encoder output into access units ready for RTP, keeping track of SPS/PPS
#[derive(Default)]
pub struct AccessUnitBuilder {
    sps: Option<Vec<u8>>,
    pps: Option<Vec<u8>>,
}

impl AccessUnitBuilder {
    pub fn parameter_sets(&self) -> Option<ParameterSets> {
        Some(ParameterSets {
            sps: self.sps.clone()?,
            pps: self.pps.clone()?,
        })
    }

    pub fn build(&mut self, frame: &EncodedFrame) -> AccessUnit {
        let mut nals: Vec<Vec<u8>> = Vec::new();
        let mut keyframe = false;
        let mut has_parameter_sets = false;

        for nal in split_annex_b(&frame.data) {
            match nal_type(nal) {
                // Delimiters carry no information over RTP
                NAL_AUD => continue,
                NAL_SPS => {
                    self.sps = Some(nal.to_vec());
                    has_parameter_sets = true;
                }
                NAL_PPS => {
                    self.pps = Some(nal.to_vec());
                    has_parameter_sets = true;
                }
                NAL_IDR => keyframe = true,
                _ => {}
            }
            nals.push(nal.to_vec());
        }

        // Every IDR carries SPS/PPS in-band, so a client joining at any keyframe can decode
        if keyframe && !has_parameter_sets {
            if let Some(params) = self.parameter_sets() {
                nals.insert(0, params.pps);
                nals.insert(0, params.sps);
            }
        }

        AccessUnit {
            nals,
            // 100ns -> 90kHz, wrapping as RTP timestamps do
            timestamp_90k: (frame.timestamp_hns * 9 / 1000) as u32,
            keyframe,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_mixed_start_codes() {
        let data = [0, 0, 0, 1, 0x67, 1, 2, 0, 0, 1, 0x68, 3, 0, 0, 0, 1, 0x65, 4, 5];
        let nals = split_annex_b(&data);
        assert_eq!(nals, vec![&[0x67, 1, 2][..], &[0x68, 3][..], &[0x65, 4, 5][..]]);
    }

    #[test]
    fn prepends_parameter_sets_on_idr() {
        let mut builder = AccessUnitBuilder::default();
        builder.build(&EncodedFrame {
            data: vec![0, 0, 0, 1, 0x67, 1, 0, 0, 0, 1, 0x68, 2, 0, 0, 0, 1, 0x65, 3],
            timestamp_hns: 0,
        });

        let au = builder.build(&EncodedFrame {
            data: vec![0, 0, 0, 1, 0x09, 0x10, 0, 0, 0, 1, 0x65, 4],
            timestamp_hns: 10_000_000,
        });

        assert!(au.keyframe);
        assert_eq!(au.nals, vec![vec![0x67, 1], vec![0x68, 2], vec![0x65, 4]]);
        assert_eq!(au.timestamp_90k, 90_000);
    }
}
