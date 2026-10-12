use std::{
    collections::hash_map::RandomState,
    hash::{BuildHasher, Hasher},
    time::SystemTime,
};

use super::h264::AccessUnit;

pub const PAYLOAD_TYPE: u8 = 96;
// Keeps packets under the usual 1500 bytes MTU, including IP/UDP/RTP headers
const MAX_PAYLOAD: usize = 1200;
const FU_A: u8 = 28;

// H.264 RTP packetization (RFC 6184), packetization-mode=1
pub struct RtpPacketizer {
    pub ssrc: u32,
    sequence: u16,
    timestamp_offset: u32,
}

impl RtpPacketizer {
    pub fn new() -> Self {
        // RFC 3550 recommends random initial values
        RtpPacketizer {
            ssrc: random_u32(),
            sequence: random_u32() as u16,
            timestamp_offset: random_u32(),
        }
    }

    pub fn next_sequence(&self) -> u16 {
        self.sequence
    }

    pub fn packetize(&mut self, access_unit: &AccessUnit) -> Vec<Vec<u8>> {
        let timestamp = access_unit.timestamp_90k.wrapping_add(self.timestamp_offset);
        let mut packets = Vec::new();

        for (index, nal) in access_unit.nals.iter().enumerate() {
            // Marker bit flags the last packet of the picture
            let last_nal = index == access_unit.nals.len() - 1;

            if nal.len() <= MAX_PAYLOAD {
                // Single NAL unit packet
                let mut packet = self.header(last_nal, timestamp, nal.len());
                packet.extend_from_slice(nal);
                packets.push(packet);
                continue;
            }

            // Fragmentation unit (FU-A): NAL header is rebuilt from indicator + FU header
            let indicator = (nal[0] & 0xE0) | FU_A;
            let nal_type = nal[0] & 0x1F;
            let chunks: Vec<&[u8]> = nal[1..].chunks(MAX_PAYLOAD - 2).collect();

            for (chunk_index, chunk) in chunks.iter().enumerate() {
                let start = chunk_index == 0;
                let end = chunk_index == chunks.len() - 1;
                let fu_header = ((start as u8) << 7) | ((end as u8) << 6) | nal_type;

                let mut packet = self.header(last_nal && end, timestamp, chunk.len() + 2);
                packet.push(indicator);
                packet.push(fu_header);
                packet.extend_from_slice(chunk);
                packets.push(packet);
            }
        }

        packets
    }

    fn header(&mut self, marker: bool, timestamp: u32, payload_len: usize) -> Vec<u8> {
        let mut packet = Vec::with_capacity(12 + payload_len);
        packet.push(0x80); // Version 2, no padding, no extension, no CSRC
        packet.push(((marker as u8) << 7) | PAYLOAD_TYPE);
        packet.extend_from_slice(&self.sequence.to_be_bytes());
        packet.extend_from_slice(&timestamp.to_be_bytes());
        packet.extend_from_slice(&self.ssrc.to_be_bytes());
        self.sequence = self.sequence.wrapping_add(1);
        packet
    }
}

// Good enough for SSRC, sequence and session ids, avoids pulling a rand crate
pub fn random_u32() -> u32 {
    let mut hasher = RandomState::new().build_hasher();
    if let Ok(now) = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH) {
        hasher.write_u128(now.as_nanos());
    }
    hasher.finish() as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fragments_large_nal_with_fu_a() {
        let mut packetizer = RtpPacketizer::new();
        let mut nal = vec![0x65];
        nal.extend(std::iter::repeat(7u8).take(3000));
        let au = AccessUnit {
            nals: vec![vec![0x67, 1], nal],
            timestamp_90k: 0,
            keyframe: true,
        };

        let packets = packetizer.packetize(&au);
        // SPS as single packet + 3000 bytes split in 3 fragments
        assert_eq!(packets.len(), 4);
        assert_eq!(packets[0][1] & 0x80, 0);
        assert_eq!(&packets[0][12..], &[0x67, 1]);

        assert_eq!(packets[1][12], 0x60 | FU_A);
        assert_eq!(packets[1][13], 0x80 | 5); // start bit
        assert_eq!(packets[3][13], 0x40 | 5); // end bit
        assert_eq!(packets[3][1] & 0x80, 0x80); // marker on last packet

        let payload: usize = packets[1..].iter().map(|p| p.len() - 14).sum();
        assert_eq!(payload, 3000);
    }
}
