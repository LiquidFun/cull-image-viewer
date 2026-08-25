//! ICC profile classification (REQUIREMENTS.md R10).
//!
//! No colour conversion happens anywhere in this program. Every file in the target
//! library is sRGB and the display is unprofiled, so the transform is the identity and
//! the renderer relies on hardware sRGB textures instead.
//!
//! What this module does is guard against *silent* wrongness: if a file ever shows up
//! carrying a wide-gamut profile, we want to say so rather than render it with the wrong
//! primaries and let the user cull on a false impression. Detection only.

/// Verdict on an embedded profile.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Verdict {
    /// No profile; EXIF said sRGB, or said nothing. Rendered as sRGB.
    AssumedSrgb,
    /// A profile that matches sRGB primaries within tolerance. Nothing to do.
    Srgb,
    /// EXIF `ColorSpace = 2`, or a profile whose primaries are not sRGB.
    /// Rendering is still sRGB, so colours will be wrong -- surface this.
    NotSrgb(&'static str),
    /// A profile we could not parse. Treated as sRGB, but worth reporting.
    Unparsable,
}

impl Verdict {
    /// True when the user should be told, because what they see may be wrong.
    pub fn needs_warning(self) -> bool {
        matches!(self, Verdict::NotSrgb(_) | Verdict::Unparsable)
    }

    pub fn describe(self) -> String {
        match self {
            Verdict::AssumedSrgb => "sRGB (assumed)".into(),
            Verdict::Srgb => "sRGB".into(),
            Verdict::NotSrgb(what) => format!("{what} - shown as sRGB, colours will be off"),
            Verdict::Unparsable => "unreadable colour profile - shown as sRGB".into(),
        }
    }
}

/// sRGB / Rec.709 primaries as D50-adapted XYZ, which is what an ICC profile stores.
/// Taken from the profiles in the target library, which match the standard exactly.
const SRGB_R: [f64; 3] = [0.4360, 0.2225, 0.0139];
const SRGB_G: [f64; 3] = [0.3851, 0.7169, 0.0971];
const SRGB_B: [f64; 3] = [0.1431, 0.0606, 0.7139];

/// Generous enough to accept the various sRGB profiles in circulation (which differ in
/// the last decimal place), tight enough to reject AdobeRGB, whose red X is 0.6097.
const TOLERANCE: f64 = 0.01;

/// Read a big-endian s15Fixed16 at `off`.
fn s15(p: &[u8], off: usize) -> Option<f64> {
    let b: [u8; 4] = p.get(off..off + 4)?.try_into().ok()?;
    Some(f64::from(i32::from_be_bytes(b)) / 65536.0)
}

/// Locate a tag's data in an ICC profile's tag table.
fn tag<'a>(profile: &'a [u8], sig: &[u8; 4]) -> Option<&'a [u8]> {
    let count = u32::from_be_bytes(profile.get(128..132)?.try_into().ok()?) as usize;
    // A profile cannot plausibly have thousands of tags; refuse absurd headers.
    if count > 1024 {
        return None;
    }
    for i in 0..count {
        let e = 132 + i * 12;
        let entry = profile.get(e..e + 12)?;
        if &entry[0..4] == sig {
            let off = u32::from_be_bytes(entry[4..8].try_into().ok()?) as usize;
            let len = u32::from_be_bytes(entry[8..12].try_into().ok()?) as usize;
            return profile.get(off..off.checked_add(len)?);
        }
    }
    None
}

/// Read an `XYZ ` type tag as a colorant triple.
fn xyz(profile: &[u8], sig: &[u8; 4]) -> Option<[f64; 3]> {
    let body = tag(profile, sig)?;
    if body.get(..4)? != b"XYZ " {
        return None;
    }
    Some([s15(body, 8)?, s15(body, 12)?, s15(body, 16)?])
}

fn close(a: [f64; 3], b: [f64; 3]) -> bool {
    a.iter().zip(b.iter()).all(|(x, y)| (x - y).abs() < TOLERANCE)
}

/// Classify an embedded ICC profile.
pub fn classify_profile(profile: &[u8]) -> Verdict {
    // Header is 128 bytes plus a 4-byte tag count.
    if profile.len() < 132 {
        return Verdict::Unparsable;
    }
    // Only matrix/TRC RGB profiles are recognised; anything else we decline to judge.
    if profile.get(16..20) != Some(b"RGB ") {
        return Verdict::Unparsable;
    }

    match (
        xyz(profile, b"rXYZ"),
        xyz(profile, b"gXYZ"),
        xyz(profile, b"bXYZ"),
    ) {
        (Some(r), Some(g), Some(b)) => {
            if close(r, SRGB_R) && close(g, SRGB_G) && close(b, SRGB_B) {
                Verdict::Srgb
            } else if (r[0] - 0.6097).abs() < 0.02 {
                Verdict::NotSrgb("Adobe RGB profile")
            } else if (r[0] - 0.7977).abs() < 0.02 {
                Verdict::NotSrgb("ProPhoto RGB profile")
            } else {
                Verdict::NotSrgb("wide-gamut profile")
            }
        }
        // A LUT-based profile has no colorant tags. We cannot cheaply judge it.
        _ => Verdict::Unparsable,
    }
}

/// Classify an image from its embedded profile and EXIF `ColorSpace` tag.
pub fn classify(icc: Option<&[u8]>, exif: Option<crate::tiff::ColorSpace>) -> Verdict {
    use crate::tiff::ColorSpace;
    match icc {
        Some(p) => classify_profile(p),
        // No profile: trust EXIF, which is what other viewers do.
        None => match exif {
            Some(ColorSpace::AdobeRgb) => Verdict::NotSrgb("EXIF says Adobe RGB"),
            _ => Verdict::AssumedSrgb,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tiff::ColorSpace;

    /// Build a minimal matrix/TRC RGB profile with the given primaries.
    fn profile(r: [f64; 3], g: [f64; 3], b: [f64; 3]) -> Vec<u8> {
        let sigs: [&[u8; 4]; 3] = [b"rXYZ", b"gXYZ", b"bXYZ"];
        let vals = [r, g, b];
        let table_len = 132 + 3 * 12;
        let mut p = vec![0u8; table_len];
        p[16..20].copy_from_slice(b"RGB ");
        p[128..132].copy_from_slice(&3u32.to_be_bytes());

        for (i, (sig, v)) in sigs.iter().zip(vals.iter()).enumerate() {
            let off = p.len();
            let mut body = Vec::new();
            body.extend_from_slice(b"XYZ ");
            body.extend_from_slice(&0u32.to_be_bytes());
            for c in v.iter() {
                body.extend_from_slice(&((c * 65536.0).round() as i32).to_be_bytes());
            }
            let e = 132 + i * 12;
            p[e..e + 4].copy_from_slice(*sig);
            p[e + 4..e + 8].copy_from_slice(&(off as u32).to_be_bytes());
            p[e + 8..e + 12].copy_from_slice(&(body.len() as u32).to_be_bytes());
            p.extend_from_slice(&body);
        }
        p
    }

    #[test]
    fn recognises_srgb_primaries() {
        let p = profile(SRGB_R, SRGB_G, SRGB_B);
        assert_eq!(classify_profile(&p), Verdict::Srgb);
        assert!(!classify_profile(&p).needs_warning());
    }

    #[test]
    fn recognises_adobe_rgb() {
        // AdobeRGB red primary.
        let p = profile([0.6097, 0.3111, 0.0195], SRGB_G, SRGB_B);
        let v = classify_profile(&p);
        assert_eq!(v, Verdict::NotSrgb("Adobe RGB profile"));
        assert!(v.needs_warning(), "must warn so the user is not misled");
    }

    #[test]
    fn recognises_prophoto() {
        let p = profile([0.7977, 0.2880, 0.0000], SRGB_G, SRGB_B);
        assert_eq!(classify_profile(&p), Verdict::NotSrgb("ProPhoto RGB profile"));
    }

    #[test]
    fn tolerates_tiny_primary_differences() {
        // Real sRGB profiles vary in the last decimal; these must still pass.
        let p = profile([0.4361, 0.2226, 0.0138], [0.3850, 0.7168, 0.0972], SRGB_B);
        assert_eq!(classify_profile(&p), Verdict::Srgb);
    }

    #[test]
    fn garbage_profiles_are_unparsable_not_panics() {
        for bytes in [vec![], vec![0u8; 10], vec![0xAB; 200], vec![0xFF; 500]] {
            let v = classify_profile(&bytes);
            assert!(
                matches!(v, Verdict::Unparsable | Verdict::NotSrgb(_)),
                "unexpected verdict {v:?}"
            );
        }
        // Every truncation of a valid profile must be handled.
        let good = profile(SRGB_R, SRGB_G, SRGB_B);
        for n in 0..good.len() {
            let _ = classify_profile(&good[..n]);
        }
    }

    #[test]
    fn absurd_tag_count_is_rejected() {
        let mut p = vec![0u8; 200];
        p[16..20].copy_from_slice(b"RGB ");
        p[128..132].copy_from_slice(&u32::MAX.to_be_bytes());
        assert_eq!(classify_profile(&p), Verdict::Unparsable);
    }

    #[test]
    fn non_rgb_profile_is_declined() {
        let mut p = profile(SRGB_R, SRGB_G, SRGB_B);
        p[16..20].copy_from_slice(b"GRAY");
        assert_eq!(classify_profile(&p), Verdict::Unparsable);
    }

    #[test]
    fn no_profile_falls_back_to_exif() {
        // The camera-JPEG case: no profile, EXIF says sRGB.
        assert_eq!(
            classify(None, Some(ColorSpace::Srgb)),
            Verdict::AssumedSrgb
        );
        assert_eq!(classify(None, None), Verdict::AssumedSrgb);
        assert!(!classify(None, Some(ColorSpace::Srgb)).needs_warning());

        let v = classify(None, Some(ColorSpace::AdobeRgb));
        assert_eq!(v, Verdict::NotSrgb("EXIF says Adobe RGB"));
        assert!(v.needs_warning());
    }

    #[test]
    fn embedded_profile_wins_over_exif() {
        // An export carrying an sRGB profile is sRGB regardless of any EXIF tag.
        let p = profile(SRGB_R, SRGB_G, SRGB_B);
        assert_eq!(
            classify(Some(&p), Some(ColorSpace::AdobeRgb)),
            Verdict::Srgb
        );
    }

    #[test]
    fn descriptions_are_human_readable() {
        assert!(classify(None, None).describe().contains("sRGB"));
        assert!(Verdict::NotSrgb("Adobe RGB profile")
            .describe()
            .contains("colours will be off"));
    }
}
