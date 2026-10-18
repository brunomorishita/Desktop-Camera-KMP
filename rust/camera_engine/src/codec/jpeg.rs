// Baseline JPEG headers (ITU-T T.81), as MJPEG cameras send them: one frame, one interleaved
// scan. Only the headers are parsed; the entropy coded data goes to the hardware decoder
// untouched, restart markers included
use std::fmt;

const SOI: u8 = 0xd8;
const EOI: u8 = 0xd9;
const SOF0: u8 = 0xc0;
const SOF1: u8 = 0xc1;
const DHT: u8 = 0xc4;
const DQT: u8 = 0xdb;
const DRI: u8 = 0xdd;
const SOS: u8 = 0xda;
const TEM: u8 = 0x01;
const RST0: u8 = 0xd0;
const RST7: u8 = 0xd7;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseError {
    Truncated,
    Invalid(&'static str),
    Unsupported(String),
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ParseError::Truncated => write!(f, "JPEG data ends too early"),
            ParseError::Invalid(what) => write!(f, "invalid JPEG {what}"),
            ParseError::Unsupported(what) => write!(f, "JPEG {what} not supported"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Component {
    pub id: u8,
    pub h_sampling: u8,
    pub v_sampling: u8,
    pub quant_table: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScanComponent {
    // Index into Frame::components
    pub index: usize,
    pub dc_table: u8,
    pub ac_table: u8,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HuffmanTable {
    // Number of codes of each length, 1 to 16 bits
    pub counts: [u8; 16],
    pub values: Vec<u8>,
}

// Chroma layout, from the sampling factors of the components
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Subsampling {
    Gray,
    Yuv420,
    Yuv422,
    Yuv422Vertical,
    Yuv444,
    Yuv411,
}

#[derive(Debug, Clone)]
pub struct Frame<'a> {
    pub width: u16,
    pub height: u16,
    pub components: Vec<Component>,
    // Coded order (zig-zag), indexed by table id
    pub quant_tables: [Option<[u8; 64]>; 4],
    // Tables missing from the picture get the defaults of Annex K, as MJPEG expects
    pub dc_tables: [Option<HuffmanTable>; 4],
    pub ac_tables: [Option<HuffmanTable>; 4],
    // MCUs between restart markers, 0 when there are none
    pub restart_interval: u16,
    pub scan: Vec<ScanComponent>,
    // Entropy coded data of the scan, up to the end of the picture
    pub scan_data: &'a [u8],
}

impl Frame<'_> {
    // Only the ratios between luma and chroma factors count: ffmpeg writes 4:2:2 as
    // 2x2,1x2,1x2 where cameras write 2x1,1x1,1x1
    pub fn subsampling(&self) -> Result<Subsampling, ParseError> {
        let [luma, cb, cr] = match self.components.as_slice() {
            [_] => return Ok(Subsampling::Gray),
            [luma, cb, cr] => [luma, cb, cr],
            _ => return Err(self.unsupported_sampling()),
        };
        let ratio = |luma: u8, chroma: u8| luma.is_multiple_of(chroma).then(|| luma / chroma);
        let ratios = (
            ratio(luma.h_sampling, cb.h_sampling),
            ratio(luma.v_sampling, cb.v_sampling),
        );
        if (cb.h_sampling, cb.v_sampling) != (cr.h_sampling, cr.v_sampling) {
            return Err(self.unsupported_sampling());
        }
        match ratios {
            (Some(1), Some(1)) => Ok(Subsampling::Yuv444),
            (Some(2), Some(1)) => Ok(Subsampling::Yuv422),
            (Some(1), Some(2)) => Ok(Subsampling::Yuv422Vertical),
            (Some(2), Some(2)) => Ok(Subsampling::Yuv420),
            (Some(4), Some(1)) => Ok(Subsampling::Yuv411),
            _ => Err(self.unsupported_sampling()),
        }
    }

    fn unsupported_sampling(&self) -> ParseError {
        let factors: Vec<String> = self
            .components
            .iter()
            .map(|component| format!("{}x{}", component.h_sampling, component.v_sampling))
            .collect();
        ParseError::Unsupported(format!("sampling {}", factors.join(",")))
    }

    // MCUs in the scan: interleaved scans cover the picture in blocks of the largest
    // sampling factors, a single component scan in 8x8 blocks of that component
    pub fn mcu_count(&self) -> u32 {
        let max_h = self
            .components
            .iter()
            .map(|c| c.h_sampling)
            .max()
            .unwrap_or(1) as u32;
        let max_v = self
            .components
            .iter()
            .map(|c| c.v_sampling)
            .max()
            .unwrap_or(1) as u32;
        let (width, height) = (self.width as u32, self.height as u32);
        if let [single] = self.scan.as_slice() {
            let component = &self.components[single.index];
            let component_width = (width * component.h_sampling as u32).div_ceil(max_h);
            let component_height = (height * component.v_sampling as u32).div_ceil(max_v);
            component_width.div_ceil(8) * component_height.div_ceil(8)
        } else {
            width.div_ceil(8 * max_h) * height.div_ceil(8 * max_v)
        }
    }
}

pub fn parse(data: &[u8]) -> Result<Frame<'_>, ParseError> {
    if data.len() < 2 || data[0] != 0xff || data[1] != SOI {
        return Err(ParseError::Invalid("start of image"));
    }
    let mut position = 2;
    let mut quant_tables = [None; 4];
    let mut dc_tables: [Option<HuffmanTable>; 4] = Default::default();
    let mut ac_tables: [Option<HuffmanTable>; 4] = Default::default();
    let mut restart_interval = 0;
    let mut header: Option<(u16, u16, Vec<Component>)> = None;

    loop {
        let marker = next_marker(data, &mut position)?;
        match marker {
            SOI | TEM | RST0..=RST7 => continue,
            EOI => return Err(ParseError::Invalid("picture without scan")),
            _ => {}
        }
        let segment = segment(data, &mut position)?;
        match marker {
            SOF0 | SOF1 => header = Some(parse_frame_header(segment)?),
            0xc2..=0xcf if marker != DHT && marker != 0xc8 && marker != 0xcc => {
                return Err(ParseError::Unsupported(format!(
                    "frame type {marker:#x} (progressive, lossless or arithmetic)"
                )))
            }
            DQT => parse_quant_tables(segment, &mut quant_tables)?,
            DHT => parse_huffman_tables(segment, &mut dc_tables, &mut ac_tables)?,
            DRI => {
                if segment.len() < 2 {
                    return Err(ParseError::Truncated);
                }
                restart_interval = u16::from_be_bytes([segment[0], segment[1]]);
            }
            SOS => {
                let (width, height, components) =
                    header.ok_or(ParseError::Invalid("scan before the frame header"))?;
                let scan = parse_scan_header(segment, &components)?;
                let scan_data = &data[position..scan_end(data, position)];

                for (slot, default) in dc_tables.iter_mut().zip(default_dc_tables()) {
                    slot.get_or_insert(default);
                }
                for (slot, default) in ac_tables.iter_mut().zip(default_ac_tables()) {
                    slot.get_or_insert(default);
                }
                let frame = Frame {
                    width,
                    height,
                    components,
                    quant_tables,
                    dc_tables,
                    ac_tables,
                    restart_interval,
                    scan,
                    scan_data,
                };
                check_tables(&frame)?;
                return Ok(frame);
            }
            // APPn, comments and the like
            _ => {}
        }
    }
}

// Skips fill bytes (0xff) and returns the marker code
fn next_marker(data: &[u8], position: &mut usize) -> Result<u8, ParseError> {
    if data.get(*position) != Some(&0xff) {
        return Err(ParseError::Invalid("marker"));
    }
    while data.get(*position) == Some(&0xff) {
        *position += 1;
    }
    let marker = *data.get(*position).ok_or(ParseError::Truncated)?;
    *position += 1;
    Ok(marker)
}

// The bytes of a marker segment, after its length field
fn segment<'a>(data: &'a [u8], position: &mut usize) -> Result<&'a [u8], ParseError> {
    let length = data
        .get(*position..*position + 2)
        .map(|bytes| u16::from_be_bytes([bytes[0], bytes[1]]) as usize)
        .ok_or(ParseError::Truncated)?;
    if length < 2 {
        return Err(ParseError::Invalid("segment length"));
    }
    let segment = data
        .get(*position + 2..*position + length)
        .ok_or(ParseError::Truncated)?;
    *position += length;
    Ok(segment)
}

// The scan ends at the first marker that is neither a stuffed 0xff00 nor a restart marker
fn scan_end(data: &[u8], start: usize) -> usize {
    let mut position = start;
    while position + 1 < data.len() {
        if data[position] == 0xff {
            match data[position + 1] {
                0x00 | RST0..=RST7 => position += 2,
                // Fill bytes before a marker
                0xff => position += 1,
                _ => return position,
            }
        } else {
            position += 1;
        }
    }
    // Some cameras cut the picture short of its EOI
    data.len()
}

fn parse_frame_header(segment: &[u8]) -> Result<(u16, u16, Vec<Component>), ParseError> {
    if segment.len() < 6 {
        return Err(ParseError::Truncated);
    }
    if segment[0] != 8 {
        return Err(ParseError::Unsupported(format!(
            "{} bit samples",
            segment[0]
        )));
    }
    let height = u16::from_be_bytes([segment[1], segment[2]]);
    let width = u16::from_be_bytes([segment[3], segment[4]]);
    // A height of 0 would be set by a DNL marker after the first scan line
    if width == 0 || height == 0 {
        return Err(ParseError::Unsupported(
            "picture size defined later".to_string(),
        ));
    }
    let count = segment[5] as usize;
    let fields = segment.get(6..6 + count * 3).ok_or(ParseError::Truncated)?;
    let components: Vec<Component> = fields
        .chunks(3)
        .map(|field| Component {
            id: field[0],
            h_sampling: field[1] >> 4,
            v_sampling: field[1] & 0xf,
            quant_table: field[2],
        })
        .collect();
    if !matches!(count, 1 | 3)
        || components.iter().any(|c| {
            !(1..=4).contains(&c.h_sampling)
                || !(1..=4).contains(&c.v_sampling)
                || c.quant_table > 3
        })
    {
        return Err(ParseError::Invalid("frame header"));
    }
    Ok((width, height, components))
}

fn parse_quant_tables(
    mut segment: &[u8],
    tables: &mut [Option<[u8; 64]>; 4],
) -> Result<(), ParseError> {
    while let Some(&info) = segment.first() {
        let (precision, id) = (info >> 4, (info & 0xf) as usize);
        if precision != 0 {
            return Err(ParseError::Unsupported(
                "16 bit quantization table".to_string(),
            ));
        }
        if id > 3 {
            return Err(ParseError::Invalid("quantization table id"));
        }
        let values = segment.get(1..65).ok_or(ParseError::Truncated)?;
        tables[id] = Some(values.try_into().unwrap());
        segment = &segment[65..];
    }
    Ok(())
}

fn parse_huffman_tables(
    mut segment: &[u8],
    dc_tables: &mut [Option<HuffmanTable>; 4],
    ac_tables: &mut [Option<HuffmanTable>; 4],
) -> Result<(), ParseError> {
    while let Some(&info) = segment.first() {
        let (class, id) = (info >> 4, (info & 0xf) as usize);
        if class > 1 || id > 3 {
            return Err(ParseError::Invalid("Huffman table id"));
        }
        let counts: [u8; 16] = segment
            .get(1..17)
            .ok_or(ParseError::Truncated)?
            .try_into()
            .unwrap();
        let total = counts.iter().map(|&count| count as usize).sum::<usize>();
        let values = segment
            .get(17..17 + total)
            .ok_or(ParseError::Truncated)?
            .to_vec();
        let table = HuffmanTable { counts, values };
        if class == 0 {
            dc_tables[id] = Some(table);
        } else {
            ac_tables[id] = Some(table);
        }
        segment = &segment[17 + total..];
    }
    Ok(())
}

fn parse_scan_header(
    segment: &[u8],
    components: &[Component],
) -> Result<Vec<ScanComponent>, ParseError> {
    let count = *segment.first().ok_or(ParseError::Truncated)? as usize;
    let fields = segment.get(1..1 + count * 2).ok_or(ParseError::Truncated)?;
    let scan = fields
        .chunks(2)
        .map(|field| {
            let index = components
                .iter()
                .position(|component| component.id == field[0])
                .ok_or(ParseError::Invalid("scan component"))?;
            Ok(ScanComponent {
                index,
                dc_table: field[1] >> 4,
                ac_table: field[1] & 0xf,
            })
        })
        .collect::<Result<Vec<_>, ParseError>>()?;
    // Pictures made of one scan per component need every scan sent to the decoder
    if scan.len() != components.len() {
        return Err(ParseError::Unsupported(
            "picture split in several scans".to_string(),
        ));
    }
    Ok(scan)
}

// Hardware decoders take two tables of each class, as baseline JPEG allows
fn check_tables(frame: &Frame) -> Result<(), ParseError> {
    for scan in &frame.scan {
        if scan.dc_table > 1 || scan.ac_table > 1 {
            return Err(ParseError::Unsupported(
                "Huffman table id above 1".to_string(),
            ));
        }
        let quant = frame.components[scan.index].quant_table as usize;
        if frame.quant_tables[quant].is_none() {
            return Err(ParseError::Invalid("missing quantization table"));
        }
    }
    Ok(())
}

// Annex K.3, tables K.3 to K.6: luminance (id 0) and chrominance (id 1)
fn default_dc_tables() -> [HuffmanTable; 2] {
    [
        HuffmanTable {
            counts: [0, 1, 5, 1, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0, 0, 0],
            values: (0..12).collect(),
        },
        HuffmanTable {
            counts: [0, 3, 1, 1, 1, 1, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0],
            values: (0..12).collect(),
        },
    ]
}

fn default_ac_tables() -> [HuffmanTable; 2] {
    [
        HuffmanTable {
            counts: [0, 2, 1, 3, 3, 2, 4, 3, 5, 5, 4, 4, 0, 0, 1, 0x7d],
            values: DEFAULT_AC_LUMINANCE.to_vec(),
        },
        HuffmanTable {
            counts: [0, 2, 1, 2, 4, 4, 3, 4, 7, 5, 4, 4, 0, 1, 2, 0x77],
            values: DEFAULT_AC_CHROMINANCE.to_vec(),
        },
    ]
}

#[rustfmt::skip]
const DEFAULT_AC_LUMINANCE: [u8; 162] = [
    0x01, 0x02, 0x03, 0x00, 0x04, 0x11, 0x05, 0x12, 0x21, 0x31, 0x41, 0x06, 0x13, 0x51, 0x61, 0x07,
    0x22, 0x71, 0x14, 0x32, 0x81, 0x91, 0xa1, 0x08, 0x23, 0x42, 0xb1, 0xc1, 0x15, 0x52, 0xd1, 0xf0,
    0x24, 0x33, 0x62, 0x72, 0x82, 0x09, 0x0a, 0x16, 0x17, 0x18, 0x19, 0x1a, 0x25, 0x26, 0x27, 0x28,
    0x29, 0x2a, 0x34, 0x35, 0x36, 0x37, 0x38, 0x39, 0x3a, 0x43, 0x44, 0x45, 0x46, 0x47, 0x48, 0x49,
    0x4a, 0x53, 0x54, 0x55, 0x56, 0x57, 0x58, 0x59, 0x5a, 0x63, 0x64, 0x65, 0x66, 0x67, 0x68, 0x69,
    0x6a, 0x73, 0x74, 0x75, 0x76, 0x77, 0x78, 0x79, 0x7a, 0x83, 0x84, 0x85, 0x86, 0x87, 0x88, 0x89,
    0x8a, 0x92, 0x93, 0x94, 0x95, 0x96, 0x97, 0x98, 0x99, 0x9a, 0xa2, 0xa3, 0xa4, 0xa5, 0xa6, 0xa7,
    0xa8, 0xa9, 0xaa, 0xb2, 0xb3, 0xb4, 0xb5, 0xb6, 0xb7, 0xb8, 0xb9, 0xba, 0xc2, 0xc3, 0xc4, 0xc5,
    0xc6, 0xc7, 0xc8, 0xc9, 0xca, 0xd2, 0xd3, 0xd4, 0xd5, 0xd6, 0xd7, 0xd8, 0xd9, 0xda, 0xe1, 0xe2,
    0xe3, 0xe4, 0xe5, 0xe6, 0xe7, 0xe8, 0xe9, 0xea, 0xf1, 0xf2, 0xf3, 0xf4, 0xf5, 0xf6, 0xf7, 0xf8,
    0xf9, 0xfa,
];

#[rustfmt::skip]
const DEFAULT_AC_CHROMINANCE: [u8; 162] = [
    0x00, 0x01, 0x02, 0x03, 0x11, 0x04, 0x05, 0x21, 0x31, 0x06, 0x12, 0x41, 0x51, 0x07, 0x61, 0x71,
    0x13, 0x22, 0x32, 0x81, 0x08, 0x14, 0x42, 0x91, 0xa1, 0xb1, 0xc1, 0x09, 0x23, 0x33, 0x52, 0xf0,
    0x15, 0x62, 0x72, 0xd1, 0x0a, 0x16, 0x24, 0x34, 0xe1, 0x25, 0xf1, 0x17, 0x18, 0x19, 0x1a, 0x26,
    0x27, 0x28, 0x29, 0x2a, 0x35, 0x36, 0x37, 0x38, 0x39, 0x3a, 0x43, 0x44, 0x45, 0x46, 0x47, 0x48,
    0x49, 0x4a, 0x53, 0x54, 0x55, 0x56, 0x57, 0x58, 0x59, 0x5a, 0x63, 0x64, 0x65, 0x66, 0x67, 0x68,
    0x69, 0x6a, 0x73, 0x74, 0x75, 0x76, 0x77, 0x78, 0x79, 0x7a, 0x82, 0x83, 0x84, 0x85, 0x86, 0x87,
    0x88, 0x89, 0x8a, 0x92, 0x93, 0x94, 0x95, 0x96, 0x97, 0x98, 0x99, 0x9a, 0xa2, 0xa3, 0xa4, 0xa5,
    0xa6, 0xa7, 0xa8, 0xa9, 0xaa, 0xb2, 0xb3, 0xb4, 0xb5, 0xb6, 0xb7, 0xb8, 0xb9, 0xba, 0xc2, 0xc3,
    0xc4, 0xc5, 0xc6, 0xc7, 0xc8, 0xc9, 0xca, 0xd2, 0xd3, 0xd4, 0xd5, 0xd6, 0xd7, 0xd8, 0xd9, 0xda,
    0xe2, 0xe3, 0xe4, 0xe5, 0xe6, 0xe7, 0xe8, 0xe9, 0xea, 0xf2, 0xf3, 0xf4, 0xf5, 0xf6, 0xf7, 0xf8,
    0xf9, 0xfa,
];

#[cfg(test)]
mod tests {
    use std::process::Command;

    use super::*;

    // A 4:2:2 picture of 16x8 pixels as UVC cameras send it: no DHT, a DRI and fill bytes
    fn sample() -> Vec<u8> {
        let mut data = vec![0xff, SOI];
        // APP0 that must be skipped
        data.extend([0xff, 0xe0, 0x00, 0x04, 0xaa, 0xbb]);
        data.extend([0xff, DQT, 0x00, 0x43, 0x00]);
        data.extend(1..=64u8);
        data.extend([0xff, DRI, 0x00, 0x04, 0x00, 0x02]);
        data.extend([
            0xff, 0xff, SOF0, 0x00, 0x11, 0x08, 0x00, 0x08, 0x00, 0x10, 0x03, 0x01, 0x21, 0x00,
            0x02, 0x11, 0x00, 0x03, 0x11, 0x00,
        ]);
        data.extend([
            0xff, SOS, 0x00, 0x0c, 0x03, 0x01, 0x00, 0x02, 0x11, 0x03, 0x11, 0x00, 0x3f, 0x00,
        ]);
        // Scan data with a stuffed 0xff and a restart marker, then the end
        data.extend([0x12, 0xff, 0x00, 0x34, 0xff, RST0, 0x56, 0xff, EOI]);
        data
    }

    #[test]
    fn parses_an_mjpeg_picture() {
        let data = sample();
        let frame = parse(&data).unwrap();
        assert_eq!((frame.width, frame.height), (16, 8));
        assert_eq!(frame.subsampling(), Ok(Subsampling::Yuv422));
        assert_eq!(frame.restart_interval, 2);
        assert_eq!(frame.quant_tables[0].unwrap()[63], 64);
        assert!(frame.quant_tables[1].is_none());
        assert_eq!(
            frame.scan[1],
            ScanComponent {
                index: 1,
                dc_table: 1,
                ac_table: 1
            }
        );
        // An MCU of 4:2:2 is 16x8
        assert_eq!(frame.mcu_count(), 1);
        let mut doubled = frame.clone();
        for component in &mut doubled.components {
            component.v_sampling = 2;
        }
        assert_eq!(doubled.subsampling(), Ok(Subsampling::Yuv422));
        doubled.components[2].h_sampling = 2;
        assert!(doubled.subsampling().is_err());
        assert_eq!(frame.scan_data, &[0x12, 0xff, 0x00, 0x34, 0xff, RST0, 0x56]);
        // Defaults filled in for the missing DHT
        assert_eq!(frame.ac_tables[1].as_ref().unwrap().values[0], 0x00);
        for table in frame.dc_tables.iter().chain(&frame.ac_tables).flatten() {
            let total: usize = table.counts.iter().map(|&count| count as usize).sum();
            assert_eq!(total, table.values.len());
        }
    }

    #[test]
    fn rejects_what_hardware_decoders_cannot_take() {
        let mut progressive = sample();
        let sof = progressive.iter().position(|&b| b == SOF0).unwrap();
        progressive[sof] = 0xc2;
        assert!(matches!(
            parse(&progressive),
            Err(ParseError::Unsupported(_))
        ));

        let data = sample();
        assert_eq!(
            parse(&data[..data.len() / 2]).unwrap_err(),
            ParseError::Truncated
        );
        assert!(parse(&[0x00, 0x01]).is_err());
    }

    #[test]
    fn counts_mcus_per_layout() {
        let data = sample();
        let mut frame = parse(&data).unwrap();
        (frame.width, frame.height) = (1280, 720);
        assert_eq!(frame.mcu_count(), 80 * 90);
        frame.components[0].v_sampling = 2;
        assert_eq!(frame.mcu_count(), 80 * 45);
        // Grayscale: one 8x8 block per MCU
        frame.components.truncate(1);
        frame.components[0] = Component {
            id: 1,
            h_sampling: 1,
            v_sampling: 1,
            quant_table: 0,
        };
        frame.scan.truncate(1);
        (frame.width, frame.height) = (17, 9);
        assert_eq!(frame.mcu_count(), 3 * 2);
    }

    // ffmpeg writes the Annex K tables in its DHT unless asked to optimize them
    #[test]
    #[ignore = "needs ffmpeg, run with: cargo test -- --ignored --nocapture"]
    fn default_tables_match_annex_k() {
        let output = Command::new("ffmpeg")
            .args(
                "-hide_banner -loglevel error -f lavfi -i testsrc2=size=64x64 -frames:v 1 \
                 -c:v mjpeg -huffman default -f mjpeg -"
                    .split_whitespace(),
            )
            .output()
            .expect("ffmpeg not found");
        assert!(output.status.success());
        let frame = parse(&output.stdout).unwrap();
        let (dc, ac) = (default_dc_tables(), default_ac_tables());
        for id in 0..2 {
            assert_eq!(frame.dc_tables[id].as_ref(), Some(&dc[id]), "DC table {id}");
            assert_eq!(frame.ac_tables[id].as_ref(), Some(&ac[id]), "AC table {id}");
        }
    }
}
