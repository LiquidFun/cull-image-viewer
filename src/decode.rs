//! Turning a file on disk into RGBA8 pixels ready for GPU upload.
//!
//! Two decoders are kept deliberately, because they have complementary strengths and
//! the choice is load-dependent:
//!
//! * [`Decoder::Zune`] -- no scaling, but emits RGBA8 directly, so there is no
//!   conversion pass before upload.
//! * [`Decoder::JpegDecoder`] -- supports DCT-domain scaling (1/2, 1/4, 1/8), which
//!   cuts IDCT work roughly by the square of the factor, but emits RGB8.
//!
//! `bench` measures both on real files; see REQUIREMENTS.md R1.

use std::io::Cursor;
use std::sync::Mutex;

use crate::tiff;

/// Recycles the large RGBA buffers decoded images live in.
///
/// A 25.6 MP frame is 102 MB, well above any allocator's mmap threshold, so a fresh one
/// is a new mapping plus ~25k page faults as the decoder writes it. Measured at 23.8 ms
/// against a ~172 ms decode; writing into a buffer that is already resident costs
/// 1.2 ms. Every frame in a shoot is the same size, so reuse is essentially always
/// possible.
///
/// Best-effort: a buffer only comes back if the renderer finishes with it (the common
/// path). Images discarded by the ring simply free theirs.
pub struct BufferPool {
    free: Mutex<Vec<Vec<u8>>>,
    /// Spare buffers held in reserve. Each is ~102 MB, so this is the memory bound.
    limit: usize,
}

impl BufferPool {
    pub fn new(limit: usize) -> Self {
        Self {
            free: Mutex::new(Vec::new()),
            limit,
        }
    }

    /// A spare buffer, or an empty one when the pool has none.
    ///
    /// Returned exactly as it came back, at its previous length: [`fit`] then finds it
    /// already the right size and does nothing. Clearing or pre-sizing here would
    /// memset 102 MB and hand most of the saving straight back.
    pub fn take(&self) -> Vec<u8> {
        self.free.lock().unwrap().pop().unwrap_or_default()
    }

    /// Hand a buffer back. Dropped if the pool is already full.
    pub fn give(&self, buf: Vec<u8>) {
        if buf.capacity() == 0 {
            return;
        }
        let mut free = self.free.lock().unwrap();
        if free.len() < self.limit {
            free.push(buf);
        }
    }

    /// Spare buffers currently held. For tests.
    pub fn spare(&self) -> usize {
        self.free.lock().unwrap().len()
    }
}

/// Make `buf` exactly `len` bytes, reusing its allocation when it can.
///
/// The three cases differ in cost by two orders of magnitude, which is the whole point
/// of the pool: an already-correct buffer is free, a too-small one is replaced by a
/// fresh zeroed mapping (the OS supplies the zeros, so there is no memset), and only
/// the in-between case pays to zero the difference.
fn fit(mut buf: Vec<u8>, len: usize) -> Vec<u8> {
    if buf.len() == len {
        return buf;
    }
    if buf.capacity() < len {
        return vec![0u8; len];
    }
    buf.resize(len, 0);
    buf
}

/// Decoded pixels plus everything the renderer needs to display them correctly.
pub struct Image {
    pub width: u32,
    pub height: u32,
    /// Tightly packed RGBA8, `width * height * 4` bytes.
    pub rgba: Vec<u8>,
    /// EXIF orientation 1..=8, applied by the renderer rather than by rotating pixels.
    pub orientation: u16,
    /// Embedded ICC profile, when the file carried one.
    pub icc: Option<Vec<u8>>,
    /// EXIF ColorSpace tag, used when there is no embedded profile.
    pub color_space: Option<tiff::ColorSpace>,
    /// Denominator actually achieved relative to native resolution (1, 2, 4, 8).
    /// The renderer needs this to know when a zoom-in requires a full-res reload.
    pub scale_denom: u32,
    /// Native dimensions before any scaling, for the same reason.
    pub native_width: u32,
    pub native_height: u32,
}

impl Image {
    pub fn megapixels(&self) -> f64 {
        f64::from(self.width) * f64::from(self.height) / 1e6
    }
}

#[derive(Debug)]
pub enum Error {
    /// No decodable image found -- not a JPEG, and no embedded preview either.
    NoImage,
    Decode(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::NoImage => write!(f, "no decodable image found"),
            Error::Decode(m) => write!(f, "decode failed: {m}"),
        }
    }
}

impl std::error::Error for Error {}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Decoder {
    Zune,
    JpegDecoder,
}

/// The JPEG bitstream to decode, located within a file's bytes.
///
/// For a plain JPEG this is the whole file. For an ARW it is the embedded
/// full-resolution preview, which avoids demosaicing entirely (REQUIREMENTS.md R9).
pub struct Located<'a> {
    pub jpeg: &'a [u8],
    pub orientation: u16,
    pub color_space: Option<tiff::ColorSpace>,
    pub icc: Option<Vec<u8>>,
    /// True when the bytes came from a RAW container's preview.
    pub from_raw: bool,
}

/// Find the JPEG bitstream inside `data`, which may be a JPEG or a TIFF-based RAW.
pub fn locate(data: &[u8]) -> Result<Located<'_>, Error> {
    // JPEG: use it directly, reading orientation and ICC from its own markers.
    if data.starts_with(&[0xFF, 0xD8, 0xFF]) {
        let meta = tiff::jpeg_meta(data);
        return Ok(Located {
            jpeg: data,
            orientation: meta.orientation,
            color_space: meta.color_space,
            icc: meta.icc,
            from_raw: false,
        });
    }

    // TIFF-based RAW: take the largest embedded preview.
    let info = tiff::parse(data).ok_or(Error::NoImage)?;
    let preview = info.largest_preview().ok_or(Error::NoImage)?;
    let jpeg = data
        .get(preview.offset..preview.offset + preview.len)
        .ok_or(Error::NoImage)?;

    // The preview is itself a JPEG and may carry its own ICC profile; prefer the
    // container's orientation, which is what other viewers honour.
    let meta = tiff::jpeg_meta(jpeg);
    let orientation = if (1..=8).contains(&preview.orientation) && preview.orientation != 1 {
        preview.orientation
    } else if info.orientation != 1 {
        info.orientation
    } else {
        meta.orientation
    };

    Ok(Located {
        jpeg,
        orientation,
        color_space: info.color_space.or(meta.color_space),
        icc: meta.icc,
        from_raw: true,
    })
}

/// Decode `data` (JPEG or RAW) to RGBA8.
///
/// `target` is a desired maximum size. When the chosen decoder supports DCT scaling,
/// the smallest power-of-two reduction that still covers `target` is used -- never
/// smaller, so the image is never upscaled to fill the viewport.
pub fn decode(data: &[u8], target: Option<(u32, u32)>, which: Decoder) -> Result<Image, Error> {
    decode_reusing(data, target, which, Vec::new())
}

/// As [`decode`], but writing into `scratch` when it is large enough.
///
/// The buffer is grown to fit if it is too small, so any buffer is safe to pass,
/// including an empty one.
pub fn decode_reusing(
    data: &[u8],
    target: Option<(u32, u32)>,
    which: Decoder,
    scratch: Vec<u8>,
) -> Result<Image, Error> {
    let loc = locate(data)?;
    let (native_w, native_h) = jpeg_dimensions(loc.jpeg).ok_or(Error::NoImage)?;

    let mut image = match which {
        Decoder::Zune => decode_zune(loc.jpeg, scratch)?,
        Decoder::JpegDecoder => decode_jpeg_decoder(loc.jpeg, target, native_w, native_h)?,
    };

    image.orientation = loc.orientation;
    image.color_space = loc.color_space;
    // A profile on the outer file wins over one inside a RAW preview.
    image.icc = loc.icc.or(image.icc);
    image.native_width = native_w;
    image.native_height = native_h;
    image.scale_denom = if image.width == 0 {
        1
    } else {
        (native_w / image.width.max(1)).max(1)
    };
    Ok(image)
}

/// Native dimensions and orientation of a file, without decoding it.
///
/// Memory-maps the file and reuses [`locate`], so only the few pages holding the header
/// and IFDs are actually read -- microseconds even for a 32 MB ARW.
///
/// The renderer needs this *before* the pixels arrive: otherwise the view is fitted to a
/// placeholder size and visibly jumps when the real image lands.
pub fn probe(path: &std::path::Path) -> Option<(u32, u32, u16)> {
    let file = std::fs::File::open(path).ok()?;
    // Safety: the mapping is read-only and dropped before returning. A concurrent
    // truncation could fault, which is why callers treat failure as non-fatal (R11).
    let map = unsafe { memmap2::Mmap::map(&file) }.ok()?;
    let loc = locate(&map).ok()?;
    let (w, h) = jpeg_dimensions(loc.jpeg)?;
    Some((w, h, loc.orientation))
}

/// Read SOF dimensions without decoding pixels.
pub fn jpeg_dimensions(data: &[u8]) -> Option<(u32, u32)> {
    let mut i = 2usize;
    while i + 4 <= data.len() {
        if data[i] != 0xFF {
            i += 1;
            continue;
        }
        let marker = data[i + 1];
        if marker == 0x01 || (0xD0..=0xD9).contains(&marker) {
            i += 2;
            continue;
        }
        let len = u16::from_be_bytes(data.get(i + 2..i + 4)?.try_into().ok()?) as usize;
        // Any SOF flavour: baseline, extended sequential, or progressive.
        if matches!(marker, 0xC0..=0xC3 | 0xC5..=0xC7 | 0xC9..=0xCB | 0xCD..=0xCF) {
            let seg = data.get(i + 4..i + 2 + len)?;
            let h = u16::from_be_bytes(seg.get(1..3)?.try_into().ok()?);
            let w = u16::from_be_bytes(seg.get(3..5)?.try_into().ok()?);
            return Some((u32::from(w), u32::from(h)));
        }
        if marker == 0xDA {
            break;
        }
        i += 2 + len;
    }
    None
}

fn decode_zune(jpeg: &[u8], scratch: Vec<u8>) -> Result<Image, Error> {
    use zune_core::bytestream::ZCursor;
    use zune_core::colorspace::ColorSpace as ZColor;
    use zune_core::options::DecoderOptions;
    use zune_jpeg::JpegDecoder;

    // Ask for RGBA up front so there is no separate widening pass before upload.
    let opts = DecoderOptions::default().jpeg_set_out_colorspace(ZColor::RGBA);
    let mut dec = JpegDecoder::new_with_options(ZCursor::new(jpeg), opts);
    // Headers first so the output size is known and `scratch` can be sized to it.
    // `decode_into` re-reads them, but that is guarded and free.
    dec.decode_headers()
        .map_err(|e| Error::Decode(e.to_string()))?;
    let size = dec.output_buffer_size().ok_or(Error::NoImage)?;
    let mut rgba = fit(scratch, size);
    dec.decode_into(&mut rgba)
        .map_err(|e| Error::Decode(e.to_string()))?;
    let info = dec.info().ok_or(Error::NoImage)?;

    Ok(Image {
        width: u32::from(info.width),
        height: u32::from(info.height),
        rgba,
        orientation: 1,
        icc: dec.icc_profile(),
        color_space: None,
        scale_denom: 1,
        native_width: u32::from(info.width),
        native_height: u32::from(info.height),
    })
}

fn decode_jpeg_decoder(
    jpeg: &[u8],
    target: Option<(u32, u32)>,
    native_w: u32,
    native_h: u32,
) -> Result<Image, Error> {
    let mut dec = jpeg_decoder::Decoder::new(Cursor::new(jpeg));

    if let Some((tw, th)) = target {
        let denom = scale_denominator(native_w, native_h, tw, th);
        if denom > 1 {
            // scale() snaps to what the DCT can actually produce and reports the result.
            let req_w = (native_w / denom).max(1) as u16;
            let req_h = (native_h / denom).max(1) as u16;
            dec.scale(req_w, req_h)
                .map_err(|e| Error::Decode(e.to_string()))?;
        }
    }

    let pixels = dec.decode().map_err(|e| Error::Decode(e.to_string()))?;
    let info = dec.info().ok_or(Error::NoImage)?;
    let (w, h) = (u32::from(info.width), u32::from(info.height));

    let rgba = match info.pixel_format {
        jpeg_decoder::PixelFormat::RGB24 => rgb_to_rgba(&pixels),
        jpeg_decoder::PixelFormat::L8 => gray_to_rgba(&pixels),
        other => return Err(Error::Decode(format!("unsupported pixel format {other:?}"))),
    };

    Ok(Image {
        width: w,
        height: h,
        rgba,
        orientation: 1,
        icc: dec.icc_profile(),
        color_space: None,
        scale_denom: 1,
        native_width: native_w,
        native_height: native_h,
    })
}

/// Largest power-of-two reduction whose result still covers `target` in both axes.
///
/// Returns 1 when no reduction applies. Capped at 8, the most the DCT can do.
pub fn scale_denominator(native_w: u32, native_h: u32, target_w: u32, target_h: u32) -> u32 {
    if target_w == 0 || target_h == 0 {
        return 1;
    }
    let mut denom = 1;
    while denom < 8 {
        let next = denom * 2;
        if native_w / next < target_w || native_h / next < target_h {
            break;
        }
        denom = next;
    }
    denom
}

fn rgb_to_rgba(rgb: &[u8]) -> Vec<u8> {
    let n = rgb.len() / 3;
    let mut out = vec![0u8; n * 4];
    for (src, dst) in rgb.chunks_exact(3).zip(out.chunks_exact_mut(4)) {
        dst[0] = src[0];
        dst[1] = src[1];
        dst[2] = src[2];
        dst[3] = 0xFF;
    }
    out
}

fn gray_to_rgba(gray: &[u8]) -> Vec<u8> {
    let mut out = vec![0u8; gray.len() * 4];
    for (&g, dst) in gray.iter().zip(out.chunks_exact_mut(4)) {
        dst[0] = g;
        dst[1] = g;
        dst[2] = g;
        dst[3] = 0xFF;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pool_reuses_the_same_allocation() {
        let pool = BufferPool::new(2);
        let mut buf = pool.take();
        assert!(buf.is_empty(), "a cold pool hands out nothing to reuse");

        buf = fit(buf, 4096);
        let addr = buf.as_ptr();
        pool.give(buf);
        assert_eq!(pool.spare(), 1);

        // Same allocation back, and already the right length so `fit` does no work.
        let again = fit(pool.take(), 4096);
        assert_eq!(again.as_ptr(), addr, "the buffer must be recycled, not realloced");
        assert_eq!(again.len(), 4096);
    }

    #[test]
    fn pool_returns_buffers_at_full_length_so_fit_is_free() {
        // The saving depends on this: a cleared buffer would be regrown, memsetting
        // 102 MB and giving most of the benefit back.
        let pool = BufferPool::new(1);
        pool.give(vec![7u8; 1024]);
        let buf = pool.take();
        assert_eq!(buf.len(), 1024, "must come back sized, not cleared");
        assert_eq!(buf[0], 7, "and must not have been zeroed on the way");
    }

    #[test]
    fn pool_is_bounded_and_ignores_empty_buffers() {
        let pool = BufferPool::new(2);
        for _ in 0..5 {
            pool.give(vec![0u8; 64]);
        }
        assert_eq!(pool.spare(), 2, "the limit is what bounds memory");

        pool.give(Vec::new());
        assert_eq!(pool.spare(), 2, "an empty buffer is not worth keeping");
    }

    #[test]
    fn fit_grows_shrinks_and_leaves_matching_buffers_alone() {
        // Too small: a fresh zeroed allocation.
        assert_eq!(fit(vec![1u8; 4], 16), vec![0u8; 16]);
        // Exact: untouched, contents and all.
        assert_eq!(fit(vec![1u8; 4], 4), vec![1u8; 4]);
        // Too big: truncated in place, keeping the allocation.
        let mut big = Vec::with_capacity(64);
        big.resize(64, 1);
        let addr = big.as_ptr();
        let out = fit(big, 8);
        assert_eq!(out.len(), 8);
        assert_eq!(out.as_ptr(), addr, "shrinking must not realloc");
    }

    #[test]
    fn decoding_into_a_pooled_buffer_matches_a_fresh_one() {
        const TINY_JPEG: &[u8] = include_bytes!("../tests/fixtures/tiny.jpg");
        let fresh = decode(TINY_JPEG, None, Decoder::Zune).unwrap();

        // A stale buffer of the wrong size, deliberately full of junk, must not leak
        // any of its old contents into the result.
        let scratch = vec![0xABu8; 17 * 9 * 4];
        let pooled = decode_reusing(TINY_JPEG, None, Decoder::Zune, scratch).unwrap();

        assert_eq!((pooled.width, pooled.height), (fresh.width, fresh.height));
        assert_eq!(pooled.rgba, fresh.rgba, "reuse must not change the pixels");
    }

    #[test]
    fn scale_denominator_never_undershoots_target() {
        // 6192x4128 native, 4K viewport: half scale is 3096x2064, still >= 2160? No --
        // 2064 < 2160, so half scale would undershoot vertically and must be rejected.
        assert_eq!(scale_denominator(6192, 4128, 3840, 2160), 1);
        // A 1440p viewport does allow half scale.
        assert_eq!(scale_denominator(6192, 4128, 2560, 1440), 2);
        // A small viewport allows a deeper reduction.
        assert_eq!(scale_denominator(6192, 4128, 800, 600), 4);
        // Never exceeds 8.
        assert_eq!(scale_denominator(6192, 4128, 1, 1), 8);
        // Degenerate targets are ignored.
        assert_eq!(scale_denominator(6192, 4128, 0, 0), 1);
    }

    #[test]
    fn rgb_widening_sets_opaque_alpha() {
        let rgba = rgb_to_rgba(&[1, 2, 3, 4, 5, 6]);
        assert_eq!(rgba, vec![1, 2, 3, 0xFF, 4, 5, 6, 0xFF]);
    }

    #[test]
    fn gray_widening_replicates_luma() {
        let rgba = gray_to_rgba(&[7, 8]);
        assert_eq!(rgba, vec![7, 7, 7, 0xFF, 8, 8, 8, 0xFF]);
    }

    #[test]
    fn locate_rejects_garbage() {
        assert!(matches!(locate(b"not an image"), Err(Error::NoImage)));
        assert!(matches!(locate(b""), Err(Error::NoImage)));
    }

    #[test]
    fn jpeg_dimensions_on_truncated_input_is_none() {
        for n in 0..12usize {
            let buf = vec![0xFFu8; n];
            assert!(jpeg_dimensions(&buf).is_none());
        }
    }
}
