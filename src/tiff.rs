//! Minimal, panic-free TIFF/EXIF reader.
//!
//! Serves two purposes:
//!   * Sony ARW: walk the top-level IFD chain to find embedded JPEG previews. The
//!     full-resolution preview lives in its own IFD with `JPEGInterchangeFormat`.
//!   * Plain JPEG: the APP1 `Exif\0\0` payload *is* a TIFF stream, so the same parser
//!     yields orientation and the `ColorSpace` tag.
//!
//! Every accessor is bounds-checked. Malformed input yields `None`, never a panic --
//! this parser is pointed at arbitrary files on disk.

/// Byte order of a TIFF stream.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Endian {
    Little,
    Big,
}

impl Endian {
    fn u16(self, b: &[u8]) -> Option<u16> {
        let a: [u8; 2] = b.get(..2)?.try_into().ok()?;
        Some(match self {
            Endian::Little => u16::from_le_bytes(a),
            Endian::Big => u16::from_be_bytes(a),
        })
    }

    fn u32(self, b: &[u8]) -> Option<u32> {
        let a: [u8; 4] = b.get(..4)?.try_into().ok()?;
        Some(match self {
            Endian::Little => u32::from_le_bytes(a),
            Endian::Big => u32::from_be_bytes(a),
        })
    }
}

// TIFF tags we care about.
const TAG_IMAGE_WIDTH: u16 = 0x0100;
const TAG_IMAGE_LENGTH: u16 = 0x0101;
const TAG_ORIENTATION: u16 = 0x0112;
const TAG_JPEG_OFFSET: u16 = 0x0201;
const TAG_JPEG_LENGTH: u16 = 0x0202;
const TAG_EXIF_IFD: u16 = 0x8769;
const TAG_COLOR_SPACE: u16 = 0xA001;

/// Guards against malformed or hostile files causing unbounded work.
const MAX_IFDS: usize = 16;
const MAX_ENTRIES: u16 = 512;

/// An embedded JPEG image found inside a TIFF container.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Preview {
    pub offset: usize,
    pub len: usize,
    /// Dimensions as declared by the containing IFD. Zero when the IFD omits them
    /// (thumbnail IFDs often do); fall back to `len` for ranking in that case.
    pub width: u32,
    pub height: u32,
    pub orientation: u16,
}

impl Preview {
    fn pixels(&self) -> u64 {
        u64::from(self.width) * u64::from(self.height)
    }
}

/// EXIF `ColorSpace` (0xA001). Camera JPEGs in the test set report `Srgb`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColorSpace {
    Srgb,
    AdobeRgb,
    /// Tag absent, or present with the "uncalibrated" sentinel 0xFFFF.
    Unknown,
}

/// What we extracted from a TIFF stream.
#[derive(Clone, Debug, Default)]
pub struct TiffInfo {
    /// EXIF orientation, 1..=8. Defaults to 1 when absent or out of range.
    pub orientation: u16,
    pub previews: Vec<Preview>,
    pub color_space: Option<ColorSpace>,
}

impl TiffInfo {
    /// The largest embedded preview, ranked by declared pixel count and then byte
    /// length. For an ARW this is the full-resolution preview.
    pub fn largest_preview(&self) -> Option<Preview> {
        self.previews
            .iter()
            .copied()
            .max_by_key(|p| (p.pixels(), p.len))
    }
}

/// Parse a TIFF stream: an ARW file, or the payload of a JPEG APP1 `Exif` segment.
///
/// `data` must start at the TIFF header (`II`/`MM` + magic).
pub fn parse(data: &[u8]) -> Option<TiffInfo> {
    let endian = match data.get(..2)? {
        b"II" => Endian::Little,
        b"MM" => Endian::Big,
        _ => return None,
    };
    // Magic 42 confirms we read the byte order correctly.
    if endian.u16(data.get(2..4)?)? != 42 {
        return None;
    }

    let mut info = TiffInfo {
        orientation: 1,
        ..Default::default()
    };
    let mut next = endian.u32(data.get(4..8)?)? as usize;
    let mut seen = Vec::new();

    // Walk the top-level IFD chain. In an ARW the previews live in sibling IFDs, so
    // the whole chain must be visited rather than stopping at IFD0.
    for _ in 0..MAX_IFDS {
        if next == 0 || seen.contains(&next) {
            break;
        }
        seen.push(next);
        let Some(link) = read_ifd(data, next, endian, &mut info) else {
            break;
        };
        next = link;
    }

    Some(info)
}

/// Read one IFD, folding its contents into `info`. Returns the offset of the next IFD.
fn read_ifd(data: &[u8], off: usize, endian: Endian, info: &mut TiffInfo) -> Option<usize> {
    let count = endian.u16(data.get(off..off + 2)?)?;
    if count > MAX_ENTRIES {
        return None;
    }

    let mut jpeg_offset = None;
    let mut jpeg_len = None;
    let mut width = 0u32;
    let mut height = 0u32;
    // Orientation is per-IFD; the preview IFD carries its own copy.
    let mut orientation = 0u16;

    for i in 0..count as usize {
        let e = off + 2 + i * 12;
        let tag = endian.u16(data.get(e..e + 2)?)?;
        let typ = endian.u16(data.get(e + 2..e + 4)?)?;
        let value = data.get(e + 8..e + 12)?;

        // Types 3 (SHORT) and 4 (LONG) both fit inline; that covers every tag here.
        let scalar = match typ {
            3 => u32::from(endian.u16(value)?),
            4 => endian.u32(value)?,
            _ => continue,
        };

        match tag {
            TAG_IMAGE_WIDTH => width = scalar,
            TAG_IMAGE_LENGTH => height = scalar,
            TAG_ORIENTATION => orientation = scalar as u16,
            TAG_JPEG_OFFSET => jpeg_offset = Some(scalar as usize),
            TAG_JPEG_LENGTH => jpeg_len = Some(scalar as usize),
            TAG_COLOR_SPACE => {
                info.color_space = Some(match scalar {
                    1 => ColorSpace::Srgb,
                    2 => ColorSpace::AdobeRgb,
                    _ => ColorSpace::Unknown,
                })
            }
            TAG_EXIF_IFD => {
                // Sub-IFD; recurse for ColorSpace. Ignore its link field.
                read_ifd(data, scalar as usize, endian, info);
            }
            _ => {}
        }
    }

    // The first orientation we encounter (IFD0's) is the authoritative one for the file.
    if info.orientation <= 1 && (1..=8).contains(&orientation) {
        info.orientation = orientation;
    }

    // Record a preview only if the pointer really lands on a JPEG SOI, so that a
    // bogus tag can never hand a garbage range to the decoder.
    if let (Some(o), Some(l)) = (jpeg_offset, jpeg_len) {
        let ends_within = o.checked_add(l).is_some_and(|end| end <= data.len());
        if l > 0 && ends_within && data.get(o..o + 3) == Some(&[0xFF, 0xD8, 0xFF]) {
            info.previews.push(Preview {
                offset: o,
                len: l,
                width,
                height,
                orientation: if (1..=8).contains(&orientation) {
                    orientation
                } else {
                    1
                },
            });
        }
    }

    let link_at = off + 2 + count as usize * 12;
    Some(endian.u32(data.get(link_at..link_at + 4)?)? as usize)
}

/// Segments of interest inside a JPEG file.
#[derive(Clone, Debug, Default)]
pub struct JpegMeta {
    pub orientation: u16,
    pub color_space: Option<ColorSpace>,
    /// Reassembled ICC profile from APP2 `ICC_PROFILE` chunks, if present.
    pub icc: Option<Vec<u8>>,
}

/// Scan a JPEG's marker segments for EXIF (APP1) and ICC (APP2).
///
/// Only the header is inspected; scanning stops at SOS, so this stays cheap even
/// when handed a whole 11 MB file.
pub fn jpeg_meta(data: &[u8]) -> JpegMeta {
    let mut meta = JpegMeta {
        orientation: 1,
        ..Default::default()
    };
    if data.get(..2) != Some(&[0xFF, 0xD8]) {
        return meta;
    }

    // ICC profiles are split across numbered APP2 chunks that must be concatenated in
    // order. Collect (index, payload) then sort, since order on disk is not guaranteed.
    let mut icc_chunks: Vec<(u8, &[u8])> = Vec::new();
    let mut i = 2usize;

    while i + 4 <= data.len() {
        if data[i] != 0xFF {
            i += 1;
            continue;
        }
        let marker = data[i + 1];
        // Standalone markers carry no length field.
        if marker == 0x01 || (0xD0..=0xD9).contains(&marker) {
            i += 2;
            continue;
        }
        // SOS: pixel data follows, nothing more to find.
        if marker == 0xDA {
            break;
        }
        let Some(len) = data
            .get(i + 2..i + 4)
            .and_then(|b| b.try_into().ok())
            .map(u16::from_be_bytes)
        else {
            break;
        };
        let len = len as usize;
        if len < 2 {
            break;
        }
        let Some(seg) = data.get(i + 4..i + 2 + len) else {
            break;
        };

        match marker {
            0xE1 if seg.starts_with(b"Exif\0\0") => {
                if let Some(t) = parse(&seg[6..]) {
                    meta.orientation = t.orientation;
                    meta.color_space = t.color_space;
                }
            }
            0xE2 if seg.starts_with(b"ICC_PROFILE\0") => {
                // Layout: "ICC_PROFILE\0" + chunk_no + chunk_total + payload.
                if let (Some(&no), Some(payload)) = (seg.get(12), seg.get(14..)) {
                    icc_chunks.push((no, payload));
                }
            }
            _ => {}
        }
        i += 2 + len;
    }

    if !icc_chunks.is_empty() {
        icc_chunks.sort_by_key(|&(no, _)| no);
        let mut icc = Vec::new();
        for (_, payload) in icc_chunks {
            icc.extend_from_slice(payload);
        }
        meta.icc = Some(icc);
    }

    meta
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_non_tiff() {
        assert!(parse(b"not a tiff at all").is_none());
        assert!(parse(b"").is_none());
        // Correct byte-order mark but wrong magic.
        assert!(parse(b"II\x00\x00\x08\x00\x00\x00").is_none());
    }

    #[test]
    fn truncated_input_does_not_panic() {
        let full = b"II\x2a\x00\x08\x00\x00\x00\x01\x00\x12\x01\x03\x00\x01\x00\x00\x00\x08\x00\x00\x00\x00\x00\x00\x00";
        // Every prefix must be handled gracefully.
        for n in 0..full.len() {
            let _ = parse(&full[..n]);
        }
    }

    #[test]
    fn reads_orientation_from_minimal_ifd() {
        // One IFD, one entry: Orientation (type SHORT) = 8.
        let mut d = Vec::new();
        d.extend_from_slice(b"II\x2a\x00");
        d.extend_from_slice(&8u32.to_le_bytes()); // IFD0 at offset 8
        d.extend_from_slice(&1u16.to_le_bytes()); // 1 entry
        d.extend_from_slice(&TAG_ORIENTATION.to_le_bytes());
        d.extend_from_slice(&3u16.to_le_bytes()); // SHORT
        d.extend_from_slice(&1u32.to_le_bytes()); // count
        d.extend_from_slice(&8u16.to_le_bytes()); // value
        d.extend_from_slice(&0u16.to_le_bytes()); // padding
        d.extend_from_slice(&0u32.to_le_bytes()); // no next IFD

        let info = parse(&d).expect("should parse");
        assert_eq!(info.orientation, 8);
        assert!(info.previews.is_empty());
    }

    #[test]
    fn ignores_preview_pointer_that_is_not_jpeg() {
        // JPEGInterchangeFormat/Length present but the target bytes are not a JPEG.
        let mut d = Vec::new();
        d.extend_from_slice(b"II\x2a\x00");
        d.extend_from_slice(&8u32.to_le_bytes());
        d.extend_from_slice(&2u16.to_le_bytes());
        for (tag, val) in [(TAG_JPEG_OFFSET, 64u32), (TAG_JPEG_LENGTH, 16u32)] {
            d.extend_from_slice(&tag.to_le_bytes());
            d.extend_from_slice(&4u16.to_le_bytes()); // LONG
            d.extend_from_slice(&1u32.to_le_bytes());
            d.extend_from_slice(&val.to_le_bytes());
        }
        d.extend_from_slice(&0u32.to_le_bytes());
        d.resize(128, 0xAA); // target region is filler, not FFD8FF

        let info = parse(&d).expect("should parse");
        assert!(
            info.previews.is_empty(),
            "must not trust a pointer that isn't a JPEG"
        );
    }

    #[test]
    fn ifd_chain_loop_terminates() {
        // IFD0's "next" points back at itself.
        let mut d = Vec::new();
        d.extend_from_slice(b"II\x2a\x00");
        d.extend_from_slice(&8u32.to_le_bytes());
        d.extend_from_slice(&0u16.to_le_bytes()); // zero entries
        d.extend_from_slice(&8u32.to_le_bytes()); // next = self
        let info = parse(&d).expect("should parse");
        assert_eq!(info.orientation, 1);
    }

    #[test]
    fn largest_preview_prefers_pixel_count() {
        let info = TiffInfo {
            orientation: 1,
            color_space: None,
            previews: vec![
                Preview { offset: 0, len: 9_000, width: 160, height: 120, orientation: 1 },
                Preview { offset: 0, len: 5_000_000, width: 6192, height: 4128, orientation: 8 },
                Preview { offset: 0, len: 460_000, width: 1616, height: 1080, orientation: 1 },
            ],
        };
        let p = info.largest_preview().unwrap();
        assert_eq!((p.width, p.height), (6192, 4128));
    }

    #[test]
    fn jpeg_meta_handles_garbage() {
        assert_eq!(jpeg_meta(b"").orientation, 1);
        assert_eq!(jpeg_meta(b"\xff\xd8").orientation, 1);
        // Declared segment length runs past the buffer.
        assert_eq!(jpeg_meta(b"\xff\xd8\xff\xe1\xff\xff").orientation, 1);
    }
}
