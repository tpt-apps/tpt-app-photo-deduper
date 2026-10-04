//! TPT Photo Deduper Pro — desktop shell (paid edition).
//!
//! A native host binary that opens the OS webview (WebView2 on Windows) and
//! serves the `trunk build --release --features pro` output of the appfront
//! DOM bundle from `dist\desktop`. The web UI renders and groups; everything
//! the browser can't do — folder dialogs, absolute paths, whole-drive walks,
//! image decoding for thumbnails, and file deletion — runs here and is
//! reached over the allowlisted IPC bridge:
//!
//! - `dialog.open`  (built-in) — native folder/drive picker
//! - `scan_start`   — begin a background walk+hash of a directory tree
//! - `scan_progress` — poll done/total/current for a running scan
//! - `scan_results` — drain the finished scan's records (hex SHA-256 +
//!   optional perceptual dHash per file)
//! - `scan_cancel`  — abort a running scan
//! - `thumbnails`   — batch data-URL previews at a requested side length
//! - `delete_files` — recycle-bin delete of the non-keepers
//!
//! Resolve order for the bundle directory:
//! 1. `TPT_PHOTO_DIST` environment variable,
//! 2. `dist\desktop` next to the current working directory (the pipeline's
//!    trunk output),
//! 3. `dist` next to the current working directory (legacy layout),
//! 4. `dist` next to the executable (portable package layout).

mod scan;

use std::path::PathBuf;

use tpt_appfront_webview::{Acl, AppBuilder, Capability, ParamKind, ParamSpec, WindowConfig};

const APP_ID: &str = "nz.co.tptsolutions.photo-deduper-pro";

fn main() {
    let dist_dir = resolve_dist_dir();
    if !dist_dir.join("index.html").exists() {
        eprintln!(
            "tpt-photo-deduper-pro: no index.html under {} — run \
             `./build.ps1 -Pro` first (or set TPT_PHOTO_DIST)",
            dist_dir.display()
        );
        std::process::exit(1);
    }

    let manager = scan::ScanManager::new();

    let result = AppBuilder::new(APP_ID)
        .with_window(WindowConfig {
            id: "main".to_string(),
            title: "TPT Photo Deduper Pro".to_string(),
            width: 1100,
            height: 800,
            dist_dir,
        })
        .with_acl(app_acl())
        .with_builtin_capabilities()
        .with_single_instance(true)
        .run_with_value_handler(move |action: &str, params: serde_json::Value| {
            handle_command(&manager, action, params)
        });

    if let Err(error) = result {
        eprintln!("tpt-photo-deduper-pro: {error:#}");
        std::process::exit(1);
    }
}

/// The app-specific IPC surface. Dialog/clipboard/notify capabilities are
/// added by `with_builtin_capabilities()`; everything that touches the real
/// filesystem is granted explicitly here, with exact parameter contracts —
/// anything else the page posts is rejected by the ACL before it reaches
/// `handle_command`.
fn app_acl() -> Acl {
    fn cap(action: &str, params: Vec<(&str, bool, ParamKind)>) -> Capability {
        Capability {
            action: action.to_string(),
            params: params
                .into_iter()
                .map(|(name, required, kind)| ParamSpec {
                    name: name.to_string(),
                    required,
                    kind,
                    default: None,
                })
                .collect(),
        }
    }
    Acl {
        capabilities: vec![
            cap(
                "scan_start",
                vec![("dir", true, ParamKind::String), ("near", false, ParamKind::Boolean)],
            ),
            cap("scan_progress", vec![("scan_id", true, ParamKind::Number)]),
            cap("scan_results", vec![("scan_id", true, ParamKind::Number)]),
            cap("scan_cancel", vec![("scan_id", true, ParamKind::Number)]),
            cap(
                "thumbnails",
                vec![("paths", true, ParamKind::Array), ("side", false, ParamKind::Number)],
            ),
            cap("delete_files", vec![("paths", true, ParamKind::Array)]),
        ],
    }
}

fn handle_command(
    manager: &scan::ScanManager,
    action: &str,
    params: serde_json::Value,
) -> Result<serde_json::Value, String> {
    match action {
        "scan_start" => {
            let dir = params
                .get("dir")
                .and_then(|value| value.as_str())
                .ok_or("scan_start: missing dir")?;
            let near = params
                .get("near")
                .and_then(|value| value.as_bool())
                .unwrap_or(false);
            if !std::path::Path::new(dir).is_dir() {
                return Err(format!("scan_start: {dir} is not a directory"));
            }
            let id = manager.start(dir, near);
            Ok(serde_json::json!({ "scan_id": id }))
        }
        "scan_progress" => {
            let id = param_u64(&params, "scan_id")? as u32;
            Ok(manager.progress(id))
        }
        "scan_results" => {
            let id = param_u64(&params, "scan_id")? as u32;
            manager
                .results(id)
                .ok_or_else(|| format!("scan_results: unknown scan id {id}"))
        }
        "scan_cancel" => {
            let id = param_u64(&params, "scan_id")? as u32;
            manager.cancel(id);
            Ok(serde_json::json!({ "cancelled": true }))
        }
        "thumbnails" => {
            let paths = params
                .get("paths")
                .and_then(|value| value.as_array())
                .ok_or("thumbnails: missing paths")?
                .iter()
                .filter_map(|value| value.as_str().map(str::to_string))
                .collect::<Vec<_>>();
            let side = params
                .get("side")
                .and_then(|value| value.as_u64())
                .unwrap_or(288)
                .clamp(16, 1600) as u32;
            Ok(manager.thumbnails(&paths, side))
        }
        "delete_files" => {
            let paths = params
                .get("paths")
                .and_then(|value| value.as_array())
                .ok_or("delete_files: missing paths")?
                .iter()
                .filter_map(|value| value.as_str().map(str::to_string))
                .collect::<Vec<_>>();
            Ok(delete_files(&paths))
        }
        other => Err(format!("unsupported action {other}")),
    }
}

fn param_u64(params: &serde_json::Value, key: &str) -> Result<u64, String> {
    params
        .get(key)
        .and_then(|value| value.as_u64())
        .ok_or_else(|| format!("missing {key}"))
}

/// Recycle-bin delete with a per-file result report. `trash::delete` moves
/// to the Recycle Bin on Windows, so a mis-click is recoverable.
fn delete_files(paths: &[String]) -> serde_json::Value {
    let mut deleted = 0u64;
    let mut failed = Vec::new();
    for path in paths {
        match trash::delete(path) {
            Ok(()) => deleted += 1,
            Err(error) => failed.push(serde_json::json!({
                "path": path,
                "error": error.to_string(),
            })),
        }
    }
    serde_json::json!({ "deleted": deleted, "failed": failed })
}

fn resolve_dist_dir() -> PathBuf {
    if let Ok(from_env) = std::env::var("TPT_PHOTO_DIST") {
        return PathBuf::from(from_env);
    }
    let mut candidates = vec![
        PathBuf::from("dist").join("desktop"),
        PathBuf::from("dist"),
    ];
    if let Ok(exe) = std::env::current_exe() {
        if let Some(exe_dir) = exe.parent() {
            candidates.push(exe_dir.join("dist"));
        }
    }
    candidates
        .into_iter()
        .find(|dir| dir.join("index.html").exists())
        .unwrap_or_else(|| PathBuf::from("dist").join("desktop"))
}
