// RBSP bit reader (ITU-T H.264 section 7.2): fixed width fields and Exp-Golomb codes
use super::ParseError;

// NAL payload without the emulation prevention bytes (00 00 03 -> 00 00)
pub fn unescape(nal: &[u8]) -> Vec<u8> {
    let mut rbsp = Vec::with_capacity(nal.len());
    let mut zeros = 0;
    for &byte in nal {
        if zeros >= 2 && byte == 3 {
            zeros = 0;
            continue;
        }
        zeros = if byte == 0 { zeros + 1 } else { 0 };
        rbsp.push(byte);
    }
    rbsp
}

// Adds the emulation prevention bytes a NAL payload needs (00 00 0x -> 00 00 03 0x)
pub fn escape(rbsp: &[u8]) -> Vec<u8> {
    let mut nal = Vec::with_capacity(rbsp.len() + rbsp.len() / 64);
    let mut zeros = 0;
    for &byte in rbsp {
        if zeros >= 2 && byte <= 3 {
            nal.push(3);
            zeros = 0;
        }
        zeros = if byte == 0 { zeros + 1 } else { 0 };
        nal.push(byte);
    }
    nal
}

// RBSP bit writer, for the parameter sets the encoder sends
#[derive(Default)]
pub struct BitWriter {
    bits: Vec<bool>,
}

impl BitWriter {
    pub fn len(&self) -> usize {
        self.bits.len()
    }

    pub fn bits(&mut self, value: u32, count: u32) -> &mut Self {
        for i in (0..count).rev() {
            self.bits.push((value >> i) & 1 == 1);
        }
        self
    }

    pub fn flag(&mut self, value: bool) -> &mut Self {
        self.bits(value as u32, 1)
    }

    pub fn ue(&mut self, value: u32) -> &mut Self {
        let code = value as u64 + 1;
        let length = 64 - code.leading_zeros();
        self.bits(0, length - 1);
        for i in (0..length).rev() {
            self.bits.push((code >> i) & 1 == 1);
        }
        self
    }

    pub fn se(&mut self, value: i32) -> &mut Self {
        self.ue(if value > 0 {
            (2 * value - 1) as u32
        } else {
            (-2 * value) as u32
        })
    }

    // rbsp_trailing_bits included
    pub fn finish(&mut self) -> Vec<u8> {
        self.bits.push(true);
        while !self.bits.len().is_multiple_of(8) {
            self.bits.push(false);
        }
        self.bits
            .chunks(8)
            .map(|chunk| chunk.iter().fold(0u8, |byte, &bit| (byte << 1) | bit as u8))
            .collect()
    }
}

pub struct BitReader<'a> {
    data: &'a [u8],
    // Position in bits from the start of `data`
    position: usize,
}

impl<'a> BitReader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        BitReader { data, position: 0 }
    }

    pub fn position(&self) -> usize {
        self.position
    }

    pub fn bit(&mut self) -> Result<bool, ParseError> {
        let byte = *self
            .data
            .get(self.position / 8)
            .ok_or(ParseError::Truncated)?;
        let bit = (byte >> (7 - self.position % 8)) & 1;
        self.position += 1;
        Ok(bit == 1)
    }

    pub fn bits(&mut self, count: u32) -> Result<u32, ParseError> {
        debug_assert!(count <= 32);
        let mut value = 0u64;
        for _ in 0..count {
            value = (value << 1) | self.bit()? as u64;
        }
        Ok(value as u32)
    }

    pub fn skip(&mut self, count: usize) -> Result<(), ParseError> {
        if self.position + count > self.data.len() * 8 {
            return Err(ParseError::Truncated);
        }
        self.position += count;
        Ok(())
    }

    // ue(v), 9.1
    pub fn ue(&mut self) -> Result<u32, ParseError> {
        let mut leading_zeros = 0;
        while !self.bit()? {
            leading_zeros += 1;
            if leading_zeros > 31 {
                return Err(ParseError::Invalid("Exp-Golomb code longer than 32 bits"));
            }
        }
        let suffix = self.bits(leading_zeros)? as u64;
        Ok(((1u64 << leading_zeros) - 1 + suffix) as u32)
    }

    // se(v), 9.1.1
    pub fn se(&mut self) -> Result<i32, ParseError> {
        let code = self.ue()? as i64;
        Ok(if code % 2 == 1 {
            ((code + 1) / 2) as i32
        } else {
            (-(code / 2)) as i32
        })
    }

    // ue(v) checked against the largest value the syntax element allows
    pub fn ue_max(&mut self, max: u32, what: &'static str) -> Result<u32, ParseError> {
        let value = self.ue()?;
        if value > max {
            return Err(ParseError::Invalid(what));
        }
        Ok(value)
    }

    // more_rbsp_data(), 7.2: anything left before the rbsp_stop_one_bit
    pub fn more_rbsp_data(&self) -> bool {
        let Some(last) = self.data.iter().rposition(|&byte| byte != 0) else {
            return false;
        };
        let stop_bit = last * 8 + 7 - self.data[last].trailing_zeros() as usize;
        self.position < stop_bit
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_exp_golomb_codes() {
        // 1 | 010 | 011 | 00100 | 00101 -> ue 0, 1, 2, 3, 4
        let data = [0b1010_0110, 0b0100_0010, 0b1000_0000];
        let mut reader = BitReader::new(&data);
        let values: Vec<u32> = (0..5).map(|_| reader.ue().unwrap()).collect();
        assert_eq!(values, [0, 1, 2, 3, 4]);

        // se: codes 1, 2, 3, 4 -> 1, -1, 2, -2
        let data = [0b0100_1100, 0b1000_0101, 0b0000_0000];
        let mut reader = BitReader::new(&data);
        let values: Vec<i32> = (0..4).map(|_| reader.se().unwrap()).collect();
        assert_eq!(values, [1, -1, 2, -2]);
    }

    #[test]
    fn removes_emulation_prevention_bytes() {
        assert_eq!(
            unescape(&[0x65, 0x00, 0x00, 0x03, 0x01, 0x00, 0x00, 0x03, 0x00, 0x03]),
            [0x65, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x03]
        );
    }

    #[test]
    fn adds_emulation_prevention_bytes() {
        let rbsp = [
            0x65, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x04, 0x00, 0x00,
        ];
        let nal = escape(&rbsp);
        assert_eq!(
            nal,
            [0x65, 0x00, 0x00, 0x03, 0x01, 0x00, 0x00, 0x03, 0x00, 0x00, 0x04, 0x00, 0x00]
        );
        assert_eq!(unescape(&nal), rbsp);
    }

    #[test]
    fn finds_the_rbsp_stop_bit() {
        // Two payload bits, then the stop bit and alignment zeros
        let data = [0b1010_0000];
        let mut reader = BitReader::new(&data);
        reader.bits(2).unwrap();
        assert!(!reader.more_rbsp_data());

        let mut reader = BitReader::new(&data);
        reader.bits(1).unwrap();
        assert!(reader.more_rbsp_data());
    }

    #[test]
    fn reports_truncated_data() {
        let mut reader = BitReader::new(&[0x00]);
        assert_eq!(reader.ue(), Err(ParseError::Truncated));
    }
}
