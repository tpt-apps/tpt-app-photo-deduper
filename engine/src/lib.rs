//! TPT Photo Deduper — engine crate (the moat).
//!
//! Duplicate-photo grouping in pure Rust with zero UI dependencies: shared
//! verbatim by the free WASM edition (exact duplicates), the paid desktop
//! edition (exact + perceptual near-duplicates), and unit-tested on the host.
//!
//! Layers:
//! - [`hash_bytes`] / [`hash_reader`] — SHA-256 content hash; byte-identical
//!   files group together no matter what format they are (including RAW).
//! - [`dhash_from_gray`] — 64-bit difference hash over an 9x8 grayscale
//!   downscale; [`hamming_distance`] scores pairs.
//! - [`group_exact`] — groups records whose content hashes collide.
//! - [`group_near`] (the Pro grouping) — union-find over records, merging
//!   identical content hashes first and then perceptual neighbours within
//!   [`DEFAULT_NEAR_THRESHOLD`] (or a caller-supplied) Hamming distance.
//!
//! The caller owns file discovery and hashing input; the engine only ever
//! sees [`PhotoRecord`]s, so the same grouping runs against browser-picked
//! `File`s and native filesystem walks alike.

mod groups;
#[cfg(feature = "phash")]
pub mod phash;

pub use groups::{group_exact, group_near, DupGroup, NearGroup};

/// Files with these extensions are considered photos worth hashing. RAW
/// formats are included: they always participate in exact-duplicate
/// grouping even though neither the browser nor this crate can decode them
/// for perceptual hashing.
pub const IMAGE_EXTENSIONS: &[&str] = &[
    "jpg", "jpeg", "jpe", "png", "gif", "webp", "bmp", "tif", "tiff", "avif", "heic", "heif",
    // RAW formats.
    "cr2", "cr3", "nef", "arw", "dng", "raf", "orf", "rw2", "srw", "pef", "3fr", "erf", "kdc",
    "dcr", "mos", "iiq", "rwl", "x3f",
];

/// The subset of [`IMAGE_EXTENSIONS`] that a browser `<img>`/`createImageBitmap`
/// can decode, i.e. the ones eligible for perceptual hashing and thumbnails.
pub const DECODABLE_EXTENSIONS: &[&str] = &[
    "jpg", "jpeg", "jpe", "png", "gif", "webp", "bmp", "avif",
];

/// Extension of `path`/`name` (case preserved — compare with
/// `eq_ignore_ascii_case`), if any.
pub fn extension_of(path: &str) -> Option<&str> {
    let name = path.rsplit(['/', '\\']).next().unwrap_or(path);
    let dot = name.rfind('.')?;
    let ext = &name[dot + 1..];
    if ext.is_empty() {
        None
    } else {
        Some(ext)
    }
}

fn matches_any(ext: &str, table: &[&str]) -> bool {
    table.iter().any(|candidate| ext.eq_ignore_ascii_case(candidate))
}

/// Whether `path` looks like a photo worth scanning.
pub fn is_image_path(path: &str) -> bool {
    match extension_of(path) {
        Some(ext) => matches_any(ext, IMAGE_EXTENSIONS),
        None => false,
    }
}

/// Whether `path` is decodable by browser image decoders (eligible for
/// perceptual hashing and thumbnail rendering).
pub fn is_decodable_path(path: &str) -> bool {
    match extension_of(path) {
        Some(ext) => matches_any(ext, DECODABLE_EXTENSIONS),
        None => false,
    }
}

/// Default Hamming-distance threshold for `group_near`: dHash distances of
/// 0-3 mean effectively identical pixels, burst shots land in the 2-10 band,
/// and unrelated photos almost always score 20+. 8 sits comfortably between
/// "misses bursts" and "false-groups distinct photos".
pub const DEFAULT_NEAR_THRESHOLD: u32 = 8;

/// One scanned file, as the caller hands it to the grouping functions.
///
/// `id` is opaque to the engine — the UI keys thumbnails, keeper selections
/// and delete lists off it. `path` is display-only (relative in the browser
/// edition, absolute in the desktop edition); `absolute_path` is set only
/// when deletion is possible (desktop). `perceptual` carries the dHash when
/// the file was decodable and near-duplicate detection is enabled.
#[derive(Debug, Clone, PartialEq)]
pub struct PhotoRecord {
    pub id: u32,
    pub name: String,
    pub path: String,
    pub absolute_path: Option<String>,
    pub size: u64,
    pub hash: [u8; 32],
    pub perceptual: Option<u64>,
}

impl PhotoRecord {
    /// Convenience constructor for tests and callers that build records
    /// programmatically (perceptual hash and absolute path default to None).
    pub fn new(id: u32, name: &str, size: u64, hash: [u8; 32]) -> Self {
        Self {
            id,
            name: name.to_string(),
            path: name.to_string(),
            absolute_path: None,
            size,
            hash,
            perceptual: None,
        }
    }
}

/// SHA-256 of a fully-buffered file.
pub fn hash_bytes(bytes: &[u8]) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher.finalize().into()
}

/// Streaming SHA-256 — the native desktop scan uses this so a multi-GB video
/// next to the photos never has to fit in memory.
pub fn hash_reader(reader: &mut impl std::io::Read) -> std::io::Result<[u8; 32]> {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    std::io::copy(reader, &mut hasher)?;
    Ok(hasher.finalize().into())
}

/// 64-bit difference hash over a row-major 9x8 grayscale buffer: each bit
/// compares a pixel with its right-hand neighbour (left-to-right, top to
/// bottom, LSB-first). Returns `None` when the buffer is not exactly 9x8.
///
/// This is the whole perceptual algorithm the Pro edition groups on — fed by
/// the browser's canvas downscale in the WASM build and by the `image` crate
/// (see `phash::phash_from_bytes`) in the native desktop build.
pub fn dhash_from_gray(gray: &[u8]) -> Option<u64> {
    const W: usize = 9;
    const H: usize = 8;
    if gray.len() != W * H {
        return None;
    }
    let mut hash = 0u64;
    for y in 0..H {
        for x in 0..W - 1 {
            if gray[y * W + x] > gray[y * W + x + 1] {
                hash |= 1u64 << (y * 8 + x);
            }
        }
    }
    Some(hash)
}

/// Number of differing bits between two perceptual hashes.
pub fn hamming_distance(a: u64, b: u64) -> u32 {
    (a ^ b).count_ones()
}

/// Converts an RGBA buffer (as handed back by a canvas `getImageData`) to
/// the row-major grayscale [`dhash_from_gray`] expects. ITU-R BT.601 luma,
/// rounded — the same weights `image::imageops::grayscale` uses, so the
/// browser and native paths produce identical hashes for identical pixels.
/// The accumulation is u32: 255·299 overflows u16, and a wrapping release
/// build would silently hash the wrong luma.
pub fn rgba_to_gray(rgba: &[u8]) -> Vec<u8> {
    rgba.chunks_exact(4)
        .map(|px| {
            let r = u32::from(px[0]);
            let g = u32::from(px[1]);
            let b = u32::from(px[2]);
            ((r * 299 + g * 587 + b * 114) / 1000) as u8
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dhash_requires_exact_9x8() {
        assert!(dhash_from_gray(&[0u8; 9 * 8]).is_some());
        assert!(dhash_from_gray(&[0u8; 9 * 7]).is_none());
    }

    #[test]
    fn dhash_uniform_image_is_zero_and_survives_int_inversion() {
        // A uniform image has no horizontal gradient: every bit 0. Brighten
        // every pixel uniformly and it must stay 0 — dHash is contrast-of-
        // neighbours, not absolute brightness.
        assert_eq!(dhash_from_gray(&[128u8; 72]), Some(0));
        assert_eq!(dhash_from_gray(&[255u8; 72]), Some(0));
    }

    #[test]
    fn dhash_detects_left_dark_right_bright_gradient() {
        // Hard vertical edge between columns 4 and 5: uniform everywhere
        // else, so exactly one comparison per row flips — the left-dark
        // image hashes to 0 (no dark>bright step), the inverted image sets
        // one bit per row, and the two are 8 bits apart.
        let mut dark_left = [40u8; 72];
        let mut bright_left = [220u8; 72];
        // Overwrite the "bright" side of each row-pair so rows flip: make a
        // hard vertical edge at x=4.
        for y in 0..8 {
            for x in 0..9 {
                let v = if x < 5 { 40 } else { 220 };
                dark_left[y * 9 + x] = v;
                bright_left[y * 9 + x] = 255 - v;
            }
        }
        let a = dhash_from_gray(&dark_left).unwrap();
        let b = dhash_from_gray(&bright_left).unwrap();
        assert_eq!(a, 0, "dark>bright never holds on the left-dark image");
        assert_eq!(hamming_distance(a, b), 8, "one flipped bit per row");
    }

    #[test]
    fn rgba_to_gray_matches_bt601_weights() {
        assert_eq!(rgba_to_gray(&[255, 0, 0, 255]), vec![76]); // 299*255/1000
        assert_eq!(rgba_to_gray(&[0, 255, 0, 255]), vec![149]); // 587*255/1000, truncated
        assert_eq!(rgba_to_gray(&[0, 0, 255, 255]), vec![29]); // 114*255/1000
        assert_eq!(rgba_to_gray(&[255, 255, 255, 255]), vec![255]);
    }

    #[test]
    fn extension_helpers() {
        assert!(is_image_path("IMG_0001.CR2"));
        assert!(is_image_path("dir/sub/photo.JPG"));
        assert!(is_image_path(r"win\dir\photo.Jpg"));
        assert!(!is_image_path("notes.txt"));
        assert!(!is_image_path("no-extension"));
        assert!(is_decodable_path("a/burst.JPG"));
        assert!(!is_decodable_path("a/burst.CR2"));
        assert_eq!(extension_of("x.y.z"), Some("z"));
    }
}
