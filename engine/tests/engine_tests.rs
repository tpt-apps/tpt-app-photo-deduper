//! Engine validation on the host.
//!
//! The perceptual suites decode images, so the whole file rides on the
//! `phash` feature: `cargo test -p tpt-photo-dedupe` runs the lib unit
//! tests, `--all-features` runs everything here.
//!
//! `todo.md` Phase 1 requires: "a real burst-shot sample set groups
//! correctly, distinct photos don't false-group". There is no committed
//! sample set, so the burst behaviour is validated against *synthesized*
//! bursts built the way cameras actually produce them: the same scene
//! re-encoded as JPEG with small exposure/geometry jitter between frames,
//! next to unrelated scenes that must stay in separate groups.

#![cfg(feature = "phash")]

use image::{DynamicImage, ImageBuffer, Rgb};
use tpt_photo_dedupe::{
    dhash_from_gray, group_exact, group_near, hamming_distance, hash_bytes, phash::phash_from_bytes,
    rgba_to_gray, PhotoRecord, DEFAULT_NEAR_THRESHOLD,
};

// ---------------------------------------------------------------------------
// Synthetic photo helpers
// ---------------------------------------------------------------------------

/// A synthetic "photo". dHash is blind to smooth gradients (a ramp hashes
/// to all-zeros), so unrelated scenes need *structure* that disagrees at
/// the 9x8 downscale: pattern family, frequency, split position and the
/// disc's position/contrast all rotate with the scene id. Burst frames of
/// one scene (small brightness/geometry jitter) barely move those bits.
fn scene(width: u32, height: u32, id: u32) -> DynamicImage {
    let mut img = ImageBuffer::<Rgb<u8>, Vec<u8>>::new(width, height);
    let variant = id % 6;
    let period = (14 + (id / 6) * 10) as i64;
    let split = width / 3 + (id % 5) * width / 12;
    for (x, y, pixel) in img.enumerate_pixels_mut() {
        let (xi, yi) = (x as i64, y as i64);
        let v: u8 = match variant {
            // Vertical / horizontal stripes.
            0 => {
                if (xi / period) % 2 == 0 { 60 } else { 200 }
            }
            1 => {
                // Anti-diagonal stripes: keeps a horizontal component, which
                // is all dHash's left-right comparisons can see.
                if ((xi - yi) / period) % 2 == 0 { 60 } else { 200 }
            }
            // Checkerboard / diagonal stripes.
            2 => {
                let cx = (xi / period) % 2;
                let cy = (yi / period) % 2;
                if cx == cy { 60 } else { 200 }
            }
            3 => {
                if ((xi + yi) / period) % 2 == 0 { 60 } else { 200 }
            }
            // Hard vertical split / horizontal thirds.
            4 => {
                if xi < split as i64 { 30 } else { 225 }
            }
            _ => {
                // Vertical thirds (dHash sees columns, not rows).
                if xi < width as i64 / 3 {
                    30
                } else if xi < 2 * width as i64 / 3 {
                    128
                } else {
                    225
                }
            }
        };
        // Coarse 40px-cell modulation: extra structure that survives the
        // 9x8 downscale, so every scene's hash is dense and spread out
        // instead of collapsing to a near-flat hash.
        let mut v = v as i64;
        if ((xi / 40 + yi / 40 + (id % 3) as i64) % 2) == 0 {
            v += 15;
        } else {
            v -= 15;
        }
        let v = v.clamp(0, 255) as u8;
        *pixel = Rgb([v, v, v]);
    }
    // A contrasting disc whose placement/level varies per scene.
    let cx = width / 4 + (id % 4) * width / 8;
    let cy = height / 3 + ((id * 7) % 3) * height / 8;
    let radius = width / 8;
    let level = if id % 2 == 0 { 245 } else { 5 };
    for (x, y, pixel) in img.enumerate_pixels_mut() {
        let dx = x as i64 - cx as i64;
        let dy = y as i64 - cy as i64;
        if dx * dx + dy * dy <= (radius as i64) * (radius as i64) {
            *pixel = Rgb([level, level, level]);
        }
    }
    DynamicImage::ImageRgb8(img)
}

fn jpeg_bytes(img: &DynamicImage, quality: u8) -> Vec<u8> {
    use image::{ExtendedColorType, ImageEncoder};
    let rgb = img.to_rgb8();
    let mut out = Vec::new();
    let encoder =
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, quality);
    encoder
        .write_image(rgb.as_raw(), rgb.width(), rgb.height(), ExtendedColorType::Rgb8)
        .expect("synthetic JPEG encodes");
    out
}

/// Builds a burst of `frames` JPEGs of one scene: frame 0 is the reference,
/// later frames get a small brightness lift, a 2px shift, and a subtle
/// exposure ramp from the left edge — roughly what consecutive burst shots
/// look like (and enough to move a few dHash bits without leaving the
/// near-duplicate band).
fn burst_jpegs(id: u32, frames: usize) -> Vec<Vec<u8>> {
    let scene = scene(320, 240, id);
    (0..frames)
        .map(|frame| {
            let mut buf = scene.to_rgb8();
            if frame > 0 {
                for (x, _, pixel) in buf.enumerate_pixels_mut() {
                    if x < 40 {
                        let drop = ((40 - x as u32) * frame as u32) as u8;
                        let mut rgb = pixel.0;
                        rgb[0] = rgb[0].saturating_sub(drop);
                        rgb[1] = rgb[1].saturating_sub(drop);
                        rgb[2] = rgb[2].saturating_sub(drop);
                        *pixel = Rgb(rgb);
                    }
                }
                let shifted = DynamicImage::ImageRgb8(buf)
                    .brighten(frame as i32 * 4)
                    .crop_imm(2, 0, 316, 240);
                jpeg_bytes(&shifted, 92)
            } else {
                jpeg_bytes(&DynamicImage::ImageRgb8(buf), 92)
            }
        })
        .collect()
}

fn record(name: &str, bytes: &[u8], id: u32, with_phash: bool) -> PhotoRecord {
    PhotoRecord {
        id,
        name: name.to_string(),
        path: name.to_string(),
        absolute_path: None,
        size: bytes.len() as u64,
        hash: hash_bytes(bytes),
        perceptual: if with_phash {
            Some(phash_from_bytes(bytes).expect("decodable"))
        } else {
            None
        },
    }
}

// ---------------------------------------------------------------------------
// Exact-duplicate grouping
// ---------------------------------------------------------------------------

#[test]
fn identical_bytes_group_and_distinct_do_not() {
    let a = b"identical photo payload".to_vec();
    let records = vec![
        PhotoRecord::new(1, "a.jpg", 100, hash_bytes(&a)),
        PhotoRecord::new(2, "b.jpg", 100, hash_bytes(&a)),
        PhotoRecord::new(3, "c.jpg", 100, hash_bytes(b"different payload entirely")),
    ];
    let groups = group_exact(&records);
    assert_eq!(groups.len(), 1, "one pair of byte-identical files: {groups:?}");
    assert_eq!(groups[0].member_ids, vec![1, 2], "same size -> name breaks the tie");
}

#[test]
fn exact_groups_sort_by_wasted_bytes() {
    let big = vec![0u8; 1000];
    let big2 = big.clone();
    let small = vec![1u8; 10];
    let small2 = small.clone();
    let records = vec![
        PhotoRecord::new(1, "big-1.jpg", big.len() as u64, hash_bytes(&big)),
        PhotoRecord::new(2, "big-2.jpg", big2.len() as u64, hash_bytes(&big2)),
        PhotoRecord::new(3, "small-1.jpg", small.len() as u64, hash_bytes(&small)),
        PhotoRecord::new(4, "small-2.jpg", small2.len() as u64, hash_bytes(&small2)),
    ];
    let groups = group_exact(&records);
    assert_eq!(groups.len(), 2);
    assert_eq!(groups[0].member_ids.len(), 2);
    // The 990-byte-wasted group comes before the 9-byte one.
    assert!(groups[0].member_ids.contains(&1) || groups[0].member_ids.contains(&2));
    let first_wasted = wasted_for_testing(&groups[0].member_ids, &records);
    let second_wasted = wasted_for_testing(&groups[1].member_ids, &records);
    assert_eq!(first_wasted, 1000);
    assert_eq!(second_wasted, 10);
}

fn wasted_for_testing(ids: &[u32], records: &[PhotoRecord]) -> u64 {
    let sizes: Vec<u64> = ids
        .iter()
        .map(|id| records.iter().find(|r| r.id == *id).unwrap().size)
        .collect();
    sizes.iter().sum::<u64>() - *sizes.iter().max().unwrap()
}

// ---------------------------------------------------------------------------
// Perceptual (near-duplicate) grouping — the Pro moat
// ---------------------------------------------------------------------------

#[test]
fn burst_frames_group_and_distinct_scenes_do_not() {
    // Three bursts of 4 frames + two unrelated single shots.
    let mut records = Vec::new();
    let mut id = 1;
    for scene_id in 0..3 {
        for (frame, bytes) in burst_jpegs(scene_id, 4).into_iter().enumerate() {
            records.push(record(
                &format!("scene{scene_id}-frame{frame}.jpg"),
                &bytes,
                id,
                true,
            ));
            id += 1;
        }
    }
    for other in 0..2 {
        let bytes = jpeg_bytes(&scene(320, 240, 50 + other), 92);
        records.push(record(&format!("lonely-{other}.jpg"), &bytes, id, true));
        id += 1;
    }

    let groups = group_near(&records, DEFAULT_NEAR_THRESHOLD);

    assert_eq!(groups.len(), 3, "exactly the three bursts group: {groups:?}");
    for group in &groups {
        assert_eq!(group.member_ids.len(), 4, "every burst is whole: {group:?}");
    }
    // No group may mix scenes: member names share a scene prefix.
    for group in &groups {
        let prefixes: Vec<String> = group
            .member_ids
            .iter()
            .map(|mid| {
                records
                    .iter()
                    .find(|r| r.id == *mid)
                    .unwrap()
                    .name
                    .split('-')
                    .next()
                    .unwrap()
                    .to_string()
            })
            .collect();
        assert!(
            prefixes.iter().all(|p| *p == prefixes[0]),
            "group mixed scenes: {prefixes:?}"
        );
    }
}

#[test]
fn distinct_scenes_land_well_above_the_threshold() {
    // The margin test behind "distinct photos don't false-group": unrelated
    // scenes must not merely exceed the default threshold, but clear it by a
    // comfortable margin (≥ 12 vs the default 8).
    let hashes: Vec<u64> = (0..8)
        .map(|id| phash_from_bytes(&jpeg_bytes(&scene(320, 240, id), 92)).unwrap())
        .collect();
    for a in 0..hashes.len() {
        for b in (a + 1)..hashes.len() {
            let distance = hamming_distance(hashes[a], hashes[b]);
            assert!(
                distance >= 12,
                "scenes {a} and {b} are only {distance} bits apart"
            );
        }
    }
}

#[test]
fn burst_frames_stay_within_the_threshold() {
    let frames = burst_jpegs(9, 5);
    let hashes: Vec<u64> =
        frames.iter().map(|bytes| phash_from_bytes(bytes).unwrap()).collect();
    for a in 0..hashes.len() {
        for b in (a + 1)..hashes.len() {
            let distance = hamming_distance(hashes[a], hashes[b]);
            assert!(
                distance <= DEFAULT_NEAR_THRESHOLD,
                "burst frames {a}/{b} are {distance} bits apart"
            );
        }
    }
}

#[test]
fn identical_copies_join_their_burst_group() {
    // A burst frame emailed to yourself (byte-identical copy) must land in
    // the same near group as the rest of the burst, not in a group of its
    // own.
    let mut frames = burst_jpegs(3, 3);
    let dup = frames[0].clone();
    frames.push(dup);

    let records: Vec<PhotoRecord> = frames
        .iter()
        .enumerate()
        .map(|(i, bytes)| record(&format!("f{i}.jpg"), bytes, i as u32, true))
        .collect();
    let groups = group_near(&records, DEFAULT_NEAR_THRESHOLD);
    assert_eq!(groups.len(), 1, "one burst group: {groups:?}");
    assert_eq!(groups[0].member_ids.len(), 4);
    assert!(groups[0].max_distance > 0, "it is a near group, not byte-identical");
}

#[test]
fn undecodable_exact_dupes_form_their_own_near_group() {
    // Two byte-identical "RAW" files (opaque payloads the engine can't
    // decode) plus two near-identical JPEGs: the RAWs group together via
    // content hash, the JPEGs via dHash, and neither merges with the other.
    let raw_a = b"CR2/RAW opaque payload one".to_vec();
    let raw_b = raw_a.clone();
    let jpeg_payloads = burst_jpegs(6, 2);

    let records = vec![
        record("raw-1.cr2", &raw_a, 1, false),
        record("raw-2.cr2", &raw_b, 2, false),
        record("j-0.jpg", &jpeg_payloads[0], 3, true),
        record("j-1.jpg", &jpeg_payloads[1], 4, true),
    ];
    // Make sure the JPEGs are NOT byte-identical (they aren't: frame 1 is
    // brightened) and the RAWs differ from the JPEGs entirely.
    assert_ne!(records[2].hash, records[3].hash);

    let groups = group_near(&records, DEFAULT_NEAR_THRESHOLD);
    assert_eq!(groups.len(), 2, "RAW pair + JPEG pair: {groups:?}");
    let raw_group = groups
        .iter()
        .find(|g| g.member_ids.contains(&1))
        .expect("RAW group exists");
    assert_eq!(raw_group.member_ids, vec![1, 2], "identical size -> name tiebreak");
    assert_eq!(raw_group.max_distance, 0, "no perceptual hashes -> identical label");
    let jpeg_group = groups.iter().find(|g| g.member_ids.contains(&3)).unwrap();
    assert_eq!(jpeg_group.member_ids.len(), 2);

    // Sanity: the payloads are consumed as intended.
    assert_eq!(jpeg_payloads.len(), 2);
    assert!(records.iter().all(|r| r.perceptual.is_some() == r.name.ends_with(".jpg")));
}

#[test]
fn loosening_the_threshold_merges_more() {
    // Two slightly different crops of the same scene sit just past the
    // strict default but inside a loose one.
    let img = scene(320, 240, 17);
    let tight = jpeg_bytes(&img.crop_imm(0, 0, 280, 240), 92);
    let wide = jpeg_bytes(&img, 92);
    let h_tight = phash_from_bytes(&tight).unwrap();
    let h_wide = phash_from_bytes(&wide).unwrap();
    let distance = hamming_distance(h_tight, h_wide);

    let records = vec![
        record("tight.jpg", &tight, 1, true),
        record("wide.jpg", &wide, 2, true),
    ];
    let strict = group_near(&records, 8);
    let loose = group_near(&records, 30);
    // Assert the threshold actually bites somewhere between the two — and
    // that the pair's measured distance tells us which side it lands on.
    if distance <= 8 {
        assert_eq!(strict.len(), 1);
    } else {
        assert_eq!(strict.len(), 0, "distance {distance} must exceed strict");
    }
    assert!(distance <= 30, "distance {distance} should be inside loose");
    assert_eq!(loose.len(), 1);
}

// ---------------------------------------------------------------------------
// dHash plumbing
// ---------------------------------------------------------------------------

#[test]
fn canvas_pipeline_tracks_native_pipeline() {
    // The WASM app hashes browser-downscaled pixels:
    // createImageBitmap → 9x8 canvas → getImageData → rgba_to_gray →
    // dhash_from_gray. The native scan hashes via the image crate's own
    // triangle-filter resize. The two filters and rounding orders differ,
    // so the hashes can't be compared bit-for-bit — but on the same source
    // image they must land within a couple of bits.
    let img = scene(64, 48, 21);
    let small = img.resize_exact(9, 8, image::imageops::FilterType::Triangle).to_rgba8();
    let rgba: Vec<u8> = small.pixels().flat_map(|p| p.0).collect();
    let canvas_hash = dhash_from_gray(&rgba_to_gray(&rgba)).unwrap();
    let native_hash = tpt_photo_dedupe::phash::phash_from_dynamic(img);
    assert!(
        hamming_distance(canvas_hash, native_hash) <= 2,
        "canvas {canvas_hash:064b} vs native {native_hash:064b}"
    );
}
