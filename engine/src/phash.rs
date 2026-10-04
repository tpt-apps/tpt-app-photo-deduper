//! Native-side perceptual hashing convenience: decode + downscale + dHash in
//! one call, used by the desktop scan and the host test-suite. The WASM app
//! uses the browser's decoders instead (canvas → [`crate::dhash_from_gray`])
//! to keep image codecs out of the wasm bundle.

use crate::dhash_from_gray;
use image::imageops::FilterType;

/// Decodes an image from memory, downscales it to 9x8 grayscale with a
/// triangle filter, and returns its 64-bit dHash. All decode errors
/// (unsupported format, truncated file, …) come back verbatim — callers
/// treat undecodable files as exact-only.
#[cfg(feature = "phash")]
pub fn phash_from_bytes(bytes: &[u8]) -> Result<u64, image::ImageError> {
    let img = image::load_from_memory(bytes)?;
    Ok(phash_from_dynamic(img))
}

/// dHash of an already-decoded image: 9x8 grayscale triangle-filter resize,
/// then [`crate::dhash_from_gray`]. Exposed so the desktop scan can hash a
/// file it decoded once for thumbnails without re-reading it.
#[cfg(feature = "phash")]
pub fn phash_from_dynamic(img: image::DynamicImage) -> u64 {
    let gray = img
        .grayscale()
        .resize_exact(9, 8, FilterType::Triangle)
        .to_luma8();
    dhash_from_gray(gray.as_raw()).unwrap_or(0)
}
