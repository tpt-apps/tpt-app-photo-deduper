//! Native background scans for the Pro desktop shell.
//!
//! One OS thread per scan: walk the tree (skipping OS/system directories),
//! count, then stream-hash every image file with the same engine the WASM
//! build uses — SHA-256 content hash always, perceptual dHash additionally
//! for decodable formats when near-duplicate detection is on. The web UI
//! polls `scan_progress` and drains `scan_results`; thumbnails are computed
//! on demand and cached in memory keyed by (path, side).

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use base64::Engine as _;
use image::ImageEncoder;
use tpt_photo_dedupe::{hash_reader, is_decodable_path, is_image_path, phash::phash_from_dynamic};

/// Directory names the whole-drive walk never descends into (case-
/// insensitive). System quarters a photo library never lives in — skipping
/// them keeps a C:\ scan from hashing a million DLLs.
const SKIP_DIRS: &[&str] = &[
    "$recycle.bin",
    "system volume information",
    "windows",
    "program files",
    "program files (x86)",
    "programdata",
    "appdata",
    "node_modules",
    ".git",
    "target",
    "$windows.~bt",
    "recovery",
];

pub struct NativeRecord {
    pub name: String,
    pub path: String,
    pub size: u64,
    /// Lowercase hex SHA-256.
    pub hash: String,
    pub phash: Option<u64>,
}

struct Job {
    cancelled: AtomicBool,
    finished: AtomicBool,
    done: AtomicUsize,
    total: AtomicUsize,
    skipped: AtomicUsize,
    current: Mutex<String>,
    error: Mutex<Option<String>>,
    records: Mutex<Option<Vec<NativeRecord>>>,
}

pub struct ScanManager {
    next_id: AtomicU32,
    jobs: Mutex<HashMap<u32, Arc<Job>>>,
    thumb_cache: Mutex<HashMap<(String, u32), String>>,
}

impl ScanManager {
    pub fn new() -> Self {
        Self {
            next_id: AtomicU32::new(1),
            jobs: Mutex::new(HashMap::new()),
            thumb_cache: Mutex::new(HashMap::new()),
        }
    }

    /// Spawns the background walk+hash thread and returns its id.
    pub fn start(&self, dir: &str, near: bool) -> u32 {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let job = Arc::new(Job {
            cancelled: AtomicBool::new(false),
            finished: AtomicBool::new(false),
            done: AtomicUsize::new(0),
            total: AtomicUsize::new(0),
            skipped: AtomicUsize::new(0),
            current: Mutex::new(dir.to_string()),
            error: Mutex::new(None),
            records: Mutex::new(None),
        });
        self.jobs.lock().expect("jobs lock").insert(id, job.clone());
        let root = PathBuf::from(dir);
        std::thread::Builder::new()
            .name(format!("photo-dedupe-scan-{id}"))
            .spawn(move || run_scan(job, root, near))
            .expect("spawn scan thread");
        id
    }

    /// Snapshot for the progress poll.
    pub fn progress(&self, id: u32) -> serde_json::Value {
        let jobs = self.jobs.lock().expect("jobs lock");
        let Some(job) = jobs.get(&id) else {
            return serde_json::json!({ "finished": true, "error": "unknown scan id" });
        };
        serde_json::json!({
            "done": job.done.load(Ordering::Relaxed),
            "total": job.total.load(Ordering::Relaxed),
            "skipped": job.skipped.load(Ordering::Relaxed),
            "current": job.current.lock().expect("current lock").clone(),
            "finished": job.finished.load(Ordering::Relaxed),
            "error": job.error.lock().expect("error lock").clone(),
        })
    }

    /// Drains the finished scan's records. Unknown or unfinished scans
    /// return `None` (mapped to an IPC error by the caller).
    pub fn results(&self, id: u32) -> Option<serde_json::Value> {
        let jobs = self.jobs.lock().expect("jobs lock");
        let job = jobs.get(&id)?;
        if !job.finished.load(Ordering::Relaxed) {
            return None;
        }
        let mut records = job.records.lock().expect("records lock").take()?;
        // Deterministic order: group display sorts by size anyway, but a
        // stable id mapping between polls keeps debugging sane.
        records.sort_by(|a, b| a.path.cmp(&b.path));
        let entries: Vec<serde_json::Value> = records
            .into_iter()
            .enumerate()
            .map(|(i, record)| {
                serde_json::json!({
                    "id": i,
                    "name": record.name,
                    "path": record.path,
                    "size": record.size,
                    "hash": record.hash,
                    "phash": record.phash,
                })
            })
            .collect();
        Some(serde_json::json!({ "records": entries }))
    }

    pub fn cancel(&self, id: u32) {
        if let Some(job) = self.jobs.lock().expect("jobs lock").get(&id) {
            job.cancelled.store(true, Ordering::Relaxed);
        }
    }

    /// Data-URL previews for a batch of paths at `side` px, cached per
    /// (path, side). Undecodable files simply have no entry — the UI
    /// renders a placeholder and stops re-asking.
    pub fn thumbnails(&self, paths: &[String], side: u32) -> serde_json::Value {
        let mut cache = self.thumb_cache.lock().expect("thumb cache lock");
        let mut thumbs = serde_json::Map::new();
        for path in paths {
            let key = (path.clone(), side);
            let url = match cache.get(&key) {
                Some(url) => Some(url.clone()),
                None => match thumbnail_data_url(Path::new(path), side) {
                    Some(url) => {
                        cache.insert(key, url.clone());
                        Some(url)
                    }
                    None => None,
                },
            };
            if let Some(url) = url {
                thumbs.insert(path.clone(), serde_json::Value::String(url));
            }
        }
        serde_json::json!({ "thumbs": thumbs })
    }
}

fn run_scan(job: Arc<Job>, root: PathBuf, near: bool) {
    // Phase 1 — walk and count, so the progress bar has a denominator.
    let mut files = Vec::new();
    walk(&root, &job, &mut files);
    if job.cancelled.load(Ordering::Relaxed) {
        return;
    }
    let total = files.len();
    job.total.store(total, Ordering::Relaxed);

    // Phase 2 — hash each image. Undreadable/undecodable files are skipped
    // individually; only a totally unexpected condition aborts the scan.
    for path in files {
        if job.cancelled.load(Ordering::Relaxed) {
            return;
        }
        *job.current.lock().expect("current lock") = path.display().to_string();

        let size = match fs::metadata(&path) {
            Ok(meta) => meta.len(),
            Err(error) => {
                note_skipped(&job, &path, &error.to_string());
                continue;
            }
        };
        let hash = match fs::File::open(&path)
            .and_then(|mut file| hash_reader(&mut file))
        {
            Ok(hash) => hex(&hash),
            Err(error) => {
                note_skipped(&job, &path, &error.to_string());
                continue;
            }
        };
        let phash = if near && is_decodable_path(&path.to_string_lossy()) {
            image::open(&path).ok().map(phash_from_dynamic)
        } else {
            None
        };

        let name = path
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_default();
        let display = path.display().to_string();
        job.records
            .lock()
            .expect("records lock")
            .get_or_insert_with(Vec::new)
            .push(NativeRecord {
                name,
                path: display,
                size,
                hash,
                phash,
            });
        job.done.fetch_add(1, Ordering::Relaxed);
    }

    job.finished.store(true, Ordering::Relaxed);
}

fn note_skipped(job: &Job, path: &Path, error: &str) {
    job.skipped.fetch_add(1, Ordering::Relaxed);
    let mut current = job.error.lock().expect("error lock");
    // First unreadable file is surfaced; the rest only bump the counter.
    if current.is_none() {
        *current = Some(format!("{} ({error})", path.display()));
    }
}

fn walk(dir: &Path, job: &Job, files: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        if job.cancelled.load(Ordering::Relaxed) {
            return;
        }
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        let path = entry.path();
        if file_type.is_dir() {
            let name = entry.file_name().to_string_lossy().to_lowercase();
            if SKIP_DIRS.contains(&name.as_str()) {
                continue;
            }
            walk(&path, job, files);
        } else if file_type.is_file() && is_image_path(&path.to_string_lossy()) {
            files.push(path);
        } else if file_type.is_file() {
            job.skipped.fetch_add(1, Ordering::Relaxed);
        }
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Downscale to fit `side` px (preserving aspect) and encode as a JPEG
/// data-URL — small enough to ship in batches over the IPC bridge.
fn thumbnail_data_url(path: &Path, side: u32) -> Option<String> {
    let img = image::open(path).ok()?;
    let thumb = img.thumbnail(side, side);
    let mut out = Vec::new();
    let encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 82);
    let rgba = thumb.to_rgba8();
    encoder
        .write_image(
            rgba.as_raw(),
            rgba.width(),
            rgba.height(),
            image::ExtendedColorType::Rgba8,
        )
        .ok()?;
    Some(format!(
        "data:image/jpeg;base64,{}",
        base64::engine::general_purpose::STANDARD.encode(out)
    ))
}
