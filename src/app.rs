// TPT Photo Deduper — UI (tpt-appfront-dom), mounted per the hub contract.
//
// Rendering architecture (mirrors the cutlist-optimizer's proven split):
// - the **source panel is mounted once** (`tpt_appfront_dom::mount`) —
//   controls never lose focus; settings live in signals;
// - the **progress and results sections re-render fine-grained** via
//   `tpt_appfront_dom::render` driven by the scan/groups signals;
// - the grouping itself is a `create_memo` over (scan outcome, near-dup
//   toggle, threshold), so re-grouping after a threshold change never
//   re-reads a single file.
//
// Two scan backends share one grouping path:
// - **Browser** (free edition on the hub, and `trunk serve` dev): hidden
//   `input[webkitdirectory]`, bytes via `File.arrayBuffer()`, perceptual
//   hashes via a 9x8 canvas fed by `createImageBitmap` — no image codecs in
//   the wasm bundle.
// - **Native** (Pro desktop shell): when the `window.__appfront` bridge is
//   present the whole scan runs in the host process (folder dialog, walk,
//   SHA-256, decode + dHash, thumbnails, delete) over the ACL-gated IPC —
//   the only mode that can resolve absolute paths and delete files.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use tpt_appfront_core::{create_memo, ContainerBuilder, Signal, UITree};
use tpt_photo_dedupe::{
    group_exact, group_near, hash_bytes, is_decodable_path, is_image_path, DupGroup, NearGroup,
    PhotoRecord, DEFAULT_NEAR_THRESHOLD,
};
#[cfg(feature = "pro")]
use tpt_photo_dedupe::rgba_to_gray;
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::JsFuture;

// ---------------------------------------------------------------------------
// Messages
// ---------------------------------------------------------------------------

// `#[allow(dead_code)]`: the free build never constructs the pro variants.
#[derive(Clone)]
#[allow(dead_code)]
enum Msg {
    /// Opens the folder picker (native dialog on the desktop shell, hidden
    /// `webkitdirectory` input in the browser).
    PickFolder,
    CancelScan,
    /// (pro) Near-duplicate detection toggle.
    SetNear(bool),
    /// (pro) Hamming threshold dropdown ("6"/"8"/"14").
    SetThreshold(String),
    /// (pro) Make `id` the keeper of the group identified by `anchor`
    /// (the group's smallest member id — stable across re-grouping).
    SetKeeper { anchor: u32, id: u32 },
    /// (pro) Open the side-by-side comparison for a group.
    CompareOpen { anchor: u32 },
    /// (pro) Cycle the comparison's right-hand candidate.
    CompareNav(i32),
    CompareClose,
    /// (pro, desktop) First click arms the batch delete; second confirms.
    DeleteArm,
    DeleteCancel,
    DeleteConfirmed,
}

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

/// Where the scan lifecycle is. `Done` keeps the engine records so a
/// threshold change re-groups without re-reading anything.
#[derive(Clone, PartialEq)]
enum ScanState {
    Idle,
    /// Browser scan in flight; native scans report through the same shape.
    Scanning(Rc<ScanProgress>),
    Failed(String),
    Done(Rc<ScanData>),
}

#[derive(Clone, PartialEq)]
struct ScanProgress {
    done: usize,
    total: usize,
    current: String,
    bytes_done: u64,
    skipped: usize,
    native: bool,
}

/// Everything the results view needs from a finished scan.
#[derive(Clone, PartialEq)]
struct ScanData {
    records: Vec<PhotoRecord>,
    skipped: usize,
    folder: String,
    /// True when the scan ran through the native bridge (delete possible,
    /// thumbnails arrive asynchronously).
    native: bool,
}

/// One duplicate cluster as the UI renders it.
#[derive(Clone, PartialEq)]
struct UiGroup {
    /// Smallest member id — stable key for keeper/compare state.
    anchor: u32,
    ids: Vec<u32>,
    wasted: u64,
    /// Every member is byte-identical (vs merely perceptually equal).
    identical: bool,
    /// (near groups) Worst Hamming distance from the group's anchor hash.
    max_distance: u32,
}

/// Memoized grouping result.
#[derive(Clone, PartialEq)]
struct GroupsView {
    groups: Rc<Vec<UiGroup>>,
    /// Clone of the scan's records — thumbnail fetches and delete lists map
    /// ids back to paths/sizes through this.
    records: Rc<Vec<PhotoRecord>>,
    scanned: usize,
    skipped: usize,
    wasted_total: u64,
    folder: String,
    native: bool,
    done: bool,
}

impl GroupsView {
    fn empty() -> Self {
        Self {
            groups: Rc::new(Vec::new()),
            records: Rc::new(Vec::new()),
            scanned: 0,
            skipped: 0,
            wasted_total: 0,
            folder: String::new(),
            native: false,
            done: false,
        }
    }
}

/// (pro) Open comparison: member ids plus which candidate the right pane
/// shows. The left pane always shows the current keeper.
#[cfg(feature = "pro")]
#[derive(Clone, PartialEq)]
struct CompareState {
    ids: Vec<u32>,
    keeper: u32,
    index: usize,
}

// ---------------------------------------------------------------------------
// mount
// ---------------------------------------------------------------------------

pub fn mount_app(container: &web_sys::Element) -> Result<(), JsValue> {
    let document = web_sys::window()
        .expect("no window")
        .document()
        .expect("no document");

    upsert_style(&document, "dedupe-app-styles", BASE_CSS);

    let scan = Signal::new(ScanState::Idle);
    #[cfg(feature = "pro")]
    let near_on = Signal::new(true);
    #[cfg(feature = "pro")]
    let threshold = Signal::new(
        storage_get("dedupe-threshold")
            .and_then(|value| value.parse::<u32>().ok())
            .filter(|value| (0..=32).contains(value))
            .unwrap_or(DEFAULT_NEAR_THRESHOLD),
    );
    let armed = Signal::new(false);
    let delete_error = Signal::new(None);
    #[cfg(feature = "pro")]
    let compare = Signal::new(None::<Rc<CompareState>>);
    // Thumbnails: id -> (side it was produced at, URL/data-URL). The browser
    // path fills this from object URLs at scan end; the native path fetches
    // data-URLs through the bridge in chunks. Bumping `thumbs_version`
    // re-renders the results (a fetch landed).
    let thumbs: Rc<RefCell<HashMap<u32, (u32, String)>>> = Rc::new(RefCell::new(HashMap::new()));
    let thumbs_version = Signal::new(0u32);
    let object_urls: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));
    // group anchor -> chosen keeper id (defaults to the first member).
    let keepers: Rc<RefCell<HashMap<u32, u32>>> = Rc::new(RefCell::new(HashMap::new()));
    // Monotonic scan generation: starting a scan invalidates any in-flight one.
    let scan_gen = Rc::new(std::cell::Cell::new(0u32));
    // How many completed scans this device has seen (free-tier upsell nudge).
    let scan_count = Signal::new(
        storage_get("dedupe-scan-count")
            .and_then(|value| value.parse::<u32>().ok())
            .unwrap_or(0),
    );
    let native_bridge = has_native_bridge();

    // Grouping memo: recomputed only when the scan outcome, the near toggle
    // or the threshold changes — never per progress tick.
    let groups = {
        let scan = scan.clone();
        #[cfg(feature = "pro")]
        let near_on = near_on.clone();
        #[cfg(feature = "pro")]
        let threshold = threshold.clone();
        create_memo(move || {
            #[cfg(feature = "pro")]
            let threshold = threshold.get();
            #[cfg(not(feature = "pro"))]
            let threshold = DEFAULT_NEAR_THRESHOLD;
            let ScanState::Done(data) = scan.get() else {
                return Rc::new(GroupsView::empty());
            };
            #[cfg(feature = "pro")]
            let near_on = near_on.get();
            #[cfg(not(feature = "pro"))]
            let near_on = false;
            Rc::new(build_groups_view(&data, near_on, threshold))
        })
    };

    // Shared dispatch: button events come back here. The source panel is
    // never re-rendered, so controls never lose focus.
    let dispatch: Rc<dyn Fn(Msg)> = {
        let scan = scan.clone();
        let armed = armed.clone();
        let delete_error = delete_error.clone();
        #[cfg(feature = "pro")]
        let near_on = near_on.clone();
        #[cfg(feature = "pro")]
        let threshold = threshold.clone();
        #[cfg(feature = "pro")]
        let compare = compare.clone();
        let thumbs = thumbs.clone();
        let thumbs_version = thumbs_version.clone();
        let keepers = keepers.clone();
        let scan_gen = scan_gen.clone();
        let scan_count = scan_count.clone();
        let groups_for_dispatch = groups.clone();
        // Two of the shadows are only touched by pro arms.
        #[cfg(not(feature = "pro"))]
        let _ = (&thumbs_version, &groups_for_dispatch);
        Rc::new(move |msg: Msg| match msg {
            Msg::PickFolder => {
                scan_gen.set(scan_gen.get() + 1);
                armed.set(false);
                delete_error.set(None);
                #[cfg(feature = "pro")]
                compare.set(None);
                let scan = scan.clone();
                let scan_gen = scan_gen.clone();
                let thumbs = thumbs.clone();
                let object_urls = object_urls.clone();
                let scan_count = scan_count.clone();
                #[cfg(feature = "pro")]
                let near_on = near_on.clone();
                if native_bridge {
                    #[cfg(feature = "pro")]
                    spawn_native_scan(
                        scan.clone(),
                        scan_gen.clone(),
                        thumbs.clone(),
                        thumbs_version.clone(),
                        scan_count.clone(),
                        near_on.get(),
                    );
                    #[cfg(not(feature = "pro"))]
                    {
                        let _ = (&thumbs, &thumbs_version, &scan_count);
                        scan.set(ScanState::Failed(
                            "The free edition scans a folder in your browser — pick a folder below."
                                .to_string(),
                        ));
                    }
                } else {
                    spawn_browser_scan(
                        scan.clone(),
                        scan_gen.clone(),
                        thumbs.clone(),
                        object_urls.clone(),
                        scan_count.clone(),
                        #[cfg(feature = "pro")]
                        near_on.clone(),
                    );
                }
            }
            Msg::CancelScan => {
                scan_gen.set(scan_gen.get() + 1);
                scan.set(ScanState::Idle);
            }
            #[cfg(feature = "pro")]
            Msg::SetNear(on) => near_on.set(on),
            #[cfg(feature = "pro")]
            Msg::SetThreshold(value) => {
                if let Ok(value) = value.trim().parse::<u32>() {
                    if (0..=32).contains(&value) {
                        storage_set("dedupe-threshold", &value.to_string());
                        threshold.set(value);
                    }
                }
            }
            #[cfg(feature = "pro")]
            Msg::SetKeeper { anchor, id } => {
                keepers.borrow_mut().insert(anchor, id);
                bump(&thumbs_version);
            }
            #[cfg(feature = "pro")]
            Msg::CompareOpen { anchor } => {
                let view = groups_for_dispatch.get();
                if let Some(group) = view.groups.iter().find(|g| g.anchor == anchor) {
                    let keeper = keeper_of(&keepers, group);
                    compare.set(Some(Rc::new(CompareState {
                        ids: group.ids.clone(),
                        keeper,
                        index: 0,
                    })));
                    // Prime the big side-by-side images; the bump re-renders
                    // the overlay when the data-URLs land.
                    let view = Rc::clone(&view);
                    let thumbs = thumbs.clone();
                    let thumbs_version = thumbs_version.clone();
                    let ids = group.ids.clone();
                    wasm_bindgen_futures::spawn_local(async move {
                        ensure_thumbs_for(&view, &thumbs, COMPARE_SIDE, &ids).await;
                        bump(&thumbs_version);
                    });
                }
            }
            #[cfg(feature = "pro")]
            Msg::CompareNav(delta) => {
                if let Some(state) = compare.get() {
                    let candidates: Vec<u32> =
                        state.ids.iter().copied().filter(|id| *id != state.keeper).collect();
                    if candidates.is_empty() {
                        return;
                    }
                    let len = candidates.len();
                    let current = state.index.min(len - 1);
                    let next = (current as i64 + delta as i64).rem_euclid(len as i64) as usize;
                    compare.set(Some(Rc::new(CompareState {
                        ids: state.ids.clone(),
                        keeper: state.keeper,
                        index: next,
                    })));
                }
            }
            #[cfg(feature = "pro")]
            Msg::CompareClose => compare.set(None),
            #[cfg(feature = "pro")]
            Msg::DeleteArm => armed.set(true),
            #[cfg(feature = "pro")]
            Msg::DeleteCancel => armed.set(false),
            Msg::DeleteConfirmed => {
                #[cfg(feature = "pro")]
                {
                    armed.set(false);
                    let scan = scan.clone();
                    let delete_error = delete_error.clone();
                    let keepers = keepers.clone();
                    let groups = groups_for_dispatch.clone();
                    spawn_delete(scan, groups, keepers, delete_error);
                }
                #[cfg(not(feature = "pro"))]
                {
                    let _ = (&armed, &delete_error, &keepers);
                }
            }
            // The pro-only messages are unreachable in the free build (and
            // already matched in the pro build — hence the allow).
            #[cfg(not(feature = "pro"))]
            #[allow(unreachable_patterns)]
            Msg::SetNear(_)
            | Msg::SetThreshold(_)
            | Msg::SetKeeper { .. }
            | Msg::CompareOpen { .. }
            | Msg::CompareNav(_)
            | Msg::CompareClose
            | Msg::DeleteArm
            | Msg::DeleteCancel
            | Msg::DeleteConfirmed => {}
        })
    };

    // 1. The static shell — mounted exactly once.
    #[cfg(feature = "pro")]
    let shell = shell_tree(threshold.get());
    #[cfg(not(feature = "pro"))]
    let shell = shell_tree();
    let root = tpt_appfront_dom::mount(container, &shell, dispatch.clone())?;
    std::mem::forget(root);

    // 2. The progress section — its own fine-grained render loop.
    let progress_el = placeholder(&container, ".pd-progress-root")?;
    {
        let scan = scan.clone();
        let scan_gen = scan_gen.clone();
        let handle = tpt_appfront_dom::render(
            &progress_el,
            Rc::new(move || progress_view(scan.clone(), scan_gen.clone())),
            dispatch.clone(),
        )?;
        std::mem::forget(handle);
    }

    // 3. The results section — grouping memo + keeper/compare/delete state.
    let results_el = placeholder(&container, ".pd-results-root")?;
    {
        let groups = groups.clone();
        let thumbs = thumbs.clone();
        let thumbs_version = thumbs_version.clone();
        let keepers = keepers.clone();
        let armed = armed.clone();
        let delete_error = delete_error.clone();
        let handle = tpt_appfront_dom::render(
            &results_el,
            Rc::new(move || {
                let _ = thumbs_version.get(); // re-render when thumbnails land
                results_view(
                    groups.get(),
                    &thumbs,
                    &keepers,
                    armed.get(),
                    delete_error.get(),
                )
            }),
            dispatch.clone(),
        )?;
        std::mem::forget(handle);
    }

    // 4. Free edition: the Pro upsell appears after the first scan.
    #[cfg(not(feature = "pro"))]
    {
        let upsell_el = placeholder(&container, ".pd-upsell-root")?;
        let scan_count_for_view = scan_count.clone();
        let handle = tpt_appfront_dom::render(
            &upsell_el,
            Rc::new(move || upsell_view(scan_count_for_view.clone())),
            dispatch.clone(),
        )?;
        std::mem::forget(handle);
    }

    // 5. Pro: the side-by-side comparison overlay.
    #[cfg(feature = "pro")]
    {
        let compare_el = placeholder(&container, ".pd-compare-root")?;
        let compare_for_view = compare.clone();
        let thumbs_for_view = thumbs.clone();
        let keepers_for_view = keepers.clone();
        let groups_for_compare = groups.clone();
        let thumbs_version_for_view = thumbs_version.clone();
        let handle = tpt_appfront_dom::render(
            &compare_el,
            Rc::new(move || {
                let _ = thumbs_version_for_view.get();
                compare_view(
                    compare_for_view.clone(),
                    groups_for_compare.get(),
                    &thumbs_for_view,
                    &keepers_for_view,
                )
            }),
            dispatch.clone(),
        )?;
        std::mem::forget(handle);
    }

    Ok(())
}

fn placeholder(container: &web_sys::Element, selector: &str) -> Result<web_sys::Element, JsValue> {
    container
        .query_selector(selector)?
        .ok_or_else(|| JsValue::from_str(&format!("{selector} placeholder missing")))
}

// ---------------------------------------------------------------------------
// Grouping
// ---------------------------------------------------------------------------

fn build_groups_view(data: &ScanData, near_on: bool, threshold: u32) -> GroupsView {
    let near_groups: Vec<NearGroup> = if near_on {
        group_near(&data.records, threshold)
    } else {
        Vec::new()
    };
    let exact_groups: Vec<DupGroup> = if near_on {
        Vec::new()
    } else {
        group_exact(&data.records)
    };

    let by_id: HashMap<u32, &PhotoRecord> =
        data.records.iter().map(|r| (r.id, r)).collect();

    let mut ui_groups: Vec<UiGroup> = Vec::new();
    let mut wasted_total = 0u64;
    let mut push_group = |ids: &[u32], max_distance: u32| {
        if ids.len() < 2 {
            return;
        }
        let mut wasted = 0u64;
        let mut max_size = 0u64;
        for id in ids {
            let size = by_id.get(id).map_or(0, |r| r.size);
            wasted += size;
            max_size = max_size.max(size);
        }
        let wasted = wasted.saturating_sub(max_size);
        let first_hash = by_id.get(&ids[0]).map(|r| r.hash);
        let identical = first_hash.is_some_and(|hash| {
            ids.iter().all(|id| by_id.get(id).is_some_and(|r| r.hash == hash))
        });
        wasted_total += wasted;
        ui_groups.push(UiGroup {
            anchor: ids.iter().copied().min().unwrap_or(0),
            ids: ids.to_vec(),
            wasted,
            identical,
            max_distance,
        });
    };

    if near_on {
        for group in &near_groups {
            push_group(&group.member_ids, group.max_distance);
        }
    } else {
        for group in &exact_groups {
            push_group(&group.member_ids, 0);
        }
    }

    GroupsView {
        groups: Rc::new(ui_groups),
        records: Rc::new(data.records.clone()),
        scanned: data.records.len(),
        skipped: data.skipped,
        wasted_total,
        folder: data.folder.clone(),
        native: data.native,
        done: true,
    }
}

fn keeper_of(keepers: &RefCell<HashMap<u32, u32>>, group: &UiGroup) -> u32 {
    keepers
        .borrow()
        .get(&group.anchor)
        .copied()
        .filter(|id| group.ids.contains(id))
        .unwrap_or(group.ids[0])
}

// ---------------------------------------------------------------------------
// Shell (mounted once)
// ---------------------------------------------------------------------------

fn shell_tree(
    #[cfg(feature = "pro")] threshold: u32,
) -> UITree<Msg> {
    let mut root_builder = ContainerBuilder::<Msg>::new();
    root_builder.container(|c| {
        c.container(|header| {
            header.heading(2, "Photo Deduper");
            #[cfg(feature = "pro")]
            header.text("Pro edition").class("pd-badge");
            #[cfg(not(feature = "pro"))]
            header.text("Free edition").class("pd-badge");
        })
        .class("pd-header");
        c.text("Find duplicate photos — everything runs locally, nothing is uploaded.")
            .class("pd-sub");

        c.container(|card| {
            card.heading(3, "1 \u{b7} Choose a folder");
            #[cfg(feature = "pro")]
            card.text("Scan a folder — or a whole drive — for duplicate and near-duplicate photos.")
                .class("pd-hint");
            #[cfg(not(feature = "pro"))]
            card.text("The free edition scans one folder for byte-identical copies: same file, different name or folder.")
                .class("pd-hint");

            card.container(|row| {
                row.button("Choose folder\u{2026}")
                    .class("pd-btn pd-primary")
                    .on_click(Msg::PickFolder);
                #[cfg(feature = "pro")]
                {
                    row.checkbox("Find near-duplicates (burst shots)", true)
                        .class("pd-near-toggle")
                        .on_toggle(|on| Msg::SetNear(on));
                    row.container(|field| {
                        field.text("Match strictness").class("pd-label");
                        field
                            .select(
                                [
                                    ("6", "Strict (6)"),
                                    ("8", "Balanced (8)"),
                                    ("14", "Loose (14)"),
                                ],
                                threshold.to_string(),
                            )
                            .on_input(|value| Msg::SetThreshold(value));
                    })
                    .class("pd-field");
                }
            })
            .class("pd-actions");

            card.container(|_| {}).class("pd-progress-root");
        })
        .class("pd-card");

        c.container(|_| {}).class("pd-results-root");

        #[cfg(not(feature = "pro"))]
        c.container(|_| {}).class("pd-upsell-root");
        #[cfg(feature = "pro")]
        c.container(|_| {}).class("pd-compare-root");
    })
    // The app styles are scoped to `.pd-app`; apply it to our own root so
    // standalone (trunk/desktop) renders styled no matter what class the
    // host container carries. Harmless when the hub already sets it.
    .class("pd-app");

    root_builder
        .into_only_child()
        .expect("shell builds exactly one root container")
}

// ---------------------------------------------------------------------------
// Views
// ---------------------------------------------------------------------------

/// Thumbnail side (px) for the group grids; the compare overlay re-fetches
/// the same files at [`COMPARE_SIDE`].
const THUMB_SIDE: u32 = 288;
#[cfg(feature = "pro")]
const COMPARE_SIDE: u32 = 900;

fn progress_view(scan: Signal<ScanState>, scan_gen: Rc<std::cell::Cell<u32>>) -> UITree<Msg> {
    let mut b = ContainerBuilder::<Msg>::new();
    match scan.get() {
        ScanState::Scanning(progress) => {
            let pct = if progress.total > 0 {
                (progress.done.min(progress.total) as f64 / progress.total as f64 * 100.0) as u32
            } else {
                0
            };
            b.container(|c| {
                c.container(|head| {
                    head.heading(
                        3,
                        format!(
                            "Scanning\u{2026} {} of {} files",
                            progress.done, progress.total
                        ),
                    );
                    head.button("Cancel").class("pd-btn").on_click(Msg::CancelScan);
                })
                .class("pd-progress-head");
                c.container(|bar| {
                    bar.container(|_| {})
                        .class("pd-bar-fill")
                        .attr("style", format!("width:{pct}%"));
                })
                .class("pd-bar");
                if !progress.current.is_empty() {
                    c.text(truncate(&progress.current, 72)).class("pd-current");
                }
                c.text(
                    if progress.native {
                        "Running in the native engine — folders, RAW files and drives are handled locally."
                    } else {
                        "Reading and hashing files in your browser. Nothing leaves this device."
                    },
                )
                .class("pd-hint");
            })
            .class("pd-card pd-progress");
        }
        ScanState::Failed(message) => {
            b.container(|c| {
                c.text(format!("Scan failed: {message}")).class("pd-error");
            })
            .class("pd-card");
        }
        _ => {}
    }
    let _ = scan_gen;
    b.into_only_child().expect("progress view")
}

fn results_view(
    view: Rc<GroupsView>,
    thumbs: &Rc<RefCell<HashMap<u32, (u32, String)>>>,
    keepers: &Rc<RefCell<HashMap<u32, u32>>>,
    armed: bool,
    delete_error: Option<String>,
) -> UITree<Msg> {
    #[cfg(not(feature = "pro"))]
    let _ = (&armed, &delete_error);
    let mut b = ContainerBuilder::<Msg>::new();
    b.container(|c| {
        c.heading(3, "2 \u{b7} Duplicates").class("pd-results-heading");
        if !view.done {
            c.text("Choose a folder to scan — duplicate groups appear here.")
                .class("pd-hint");
        } else if view.groups.is_empty() {
            c.text(format!(
                "No duplicates found — {} photo{} scanned{}.",
                view.scanned,
                if view.scanned == 1 { "" } else { "s" },
                if view.skipped > 0 {
                    format!(" ({} non-image file{} ignored)", view.skipped, if view.skipped == 1 { "" } else { "s" })
                } else {
                    String::new()
                },
            ))
            .class("pd-empty");
        } else {
            let redundant: usize = view.groups.iter().map(|g| g.ids.len() - 1).sum();
            c.text(format!(
                "{} duplicate group{} \u{b7} {} redundant file{} \u{b7} {} reclaimable",
                view.groups.len(),
                if view.groups.len() == 1 { "" } else { "s" },
                redundant,
                if redundant == 1 { "" } else { "s" },
                human_size(view.wasted_total),
            ))
            .class("pd-headline");
            if !view.folder.is_empty() {
                c.text(truncate(&view.folder, 90)).class("pd-folder");
            }

            // Stats strip.
            c.container(|stats| {
                stat_tile(stats, &view.groups.len().to_string(), "groups");
                stat_tile(stats, &redundant.to_string(), "redundant files");
                stat_tile(stats, &human_size(view.wasted_total), "reclaimable");
            })
            .class("pd-stats");

            #[cfg(feature = "pro")]
            delete_bar(c, &view, keepers, armed);

            if let Some(error) = delete_error {
                c.text(error).class("pd-error");
            }

            for group in view.groups.iter() {
                c.container(|card| group_card(card, &view, group, thumbs, keepers))
                    .class("pd-group");
            }
        }
    })
    .class("pd-card pd-results");
    b.into_only_child().expect("results view")
}

fn stat_tile(c: &mut ContainerBuilder<Msg>, value: &str, label: &str) {
    c.container(|tile| {
        tile.text(value.to_string()).class("pd-stat-value");
        tile.text(label.to_string()).class("pd-stat-label");
    })
    .class("pd-stat");
}

fn group_card(
    c: &mut ContainerBuilder<Msg>,
    view: &GroupsView,
    group: &UiGroup,
    thumbs: &Rc<RefCell<HashMap<u32, (u32, String)>>>,
    keepers: &Rc<RefCell<HashMap<u32, u32>>>,
) {
    let keeper = keeper_of(keepers, group);
    c.container(|head| {
        head.container(|badges| {
            let label = if group.identical {
                "Identical copies".to_string()
            } else {
                format!("Near-duplicates ({} bits apart)", group.max_distance)
            };
            badges.text(label).class(if group.identical { "pd-badge pd-badge-identical" } else { "pd-badge pd-badge-near" });
            badges
                .text(format!(
                    "{} files \u{b7} {} reclaimable",
                    group.ids.len(),
                    human_size(group.wasted)
                ))
                .class("pd-group-meta");
        })
        .class("pd-group-head");
        #[cfg(feature = "pro")]
        {
            if group.ids.len() >= 2 {
                head.button("Compare")
                    .class("pd-btn pd-ghost")
                    .on_click(Msg::CompareOpen { anchor: group.anchor });
            }
        }
    })
    .class("pd-group-top");

    let by_id: HashMap<u32, &PhotoRecord> =
        view.records.iter().map(|r| (r.id, r)).collect();
    for id in &group.ids {
        let record = by_id.get(id).expect("group member present");
        let is_keeper = *id == keeper;
        c.container(|row| {
            thumb_node(row, thumbs, *id, &record.name);
            row.container(|info| {
                info.text(record.name.clone()).class("pd-name");
                info.text(record.path.clone()).class("pd-path");
                info.text(human_size(record.size)).class("pd-size");
            })
            .class("pd-fileinfo");
            #[cfg(feature = "pro")]
            {
                if is_keeper {
                    row.text("Keeper").class("pd-keeper-tag");
                } else {
                    row.button("Keep this one")
                        .class("pd-btn pd-ghost")
                        .on_click(Msg::SetKeeper { anchor: group.anchor, id: *id });
                }
            }
            #[cfg(not(feature = "pro"))]
            {
                if is_keeper {
                    row.text("Keeper").class("pd-keeper-tag");
                }
            }
        })
        .class(if is_keeper { "pd-row pd-row-keeper" } else { "pd-row" })
        .key(record.path.clone());
    }
}

fn thumb_node(
    c: &mut ContainerBuilder<Msg>,
    thumbs: &Rc<RefCell<HashMap<u32, (u32, String)>>>,
    id: u32,
    name: &str,
) {
    let existing = thumbs.borrow().get(&id).cloned();
    match existing {
        Some((_side, url)) => {
            c.image(url, format!("Thumbnail of {name}")).class("pd-thumb");
        }
        None => {
            c.container(|_| {})
                .class("pd-thumb pd-thumb-empty")
                .aria("label", "No preview available");
        }
    }
}

/// (pro) The batch-delete action bar above the group list.
#[cfg(feature = "pro")]
fn delete_bar(
    c: &mut ContainerBuilder<Msg>,
    view: &GroupsView,
    keepers: &Rc<RefCell<HashMap<u32, u32>>>,
    armed: bool,
) {
    let deletable = deletable_files(view, keepers);
    let count = deletable.len();
    let bytes: u64 = deletable.iter().map(|r| r.size).sum();
    c.container(|bar| {
        if !view.native {
            bar.text("Batch delete runs in the desktop edition — it needs real file access.")
                .class("pd-hint pd-delete-note");
        } else if count == 0 {
            bar.text("Pick the keeper in each group to enable batch delete.")
                .class("pd-hint");
        } else if armed {
            bar.text(format!("Delete {count} file{} ({})? This cannot be undone.", if count == 1 { "" } else { "s" }, human_size(bytes)))
                .class("pd-arm-warning");
            bar.button("Yes, delete")
                .class("pd-btn pd-danger")
                .on_click(Msg::DeleteConfirmed);
            bar.button("Cancel").class("pd-btn").on_click(Msg::DeleteCancel);
        } else {
            bar.button(format!("Delete {count} non-keeper{} ({})\u{2026}", if count == 1 { "" } else { "s" }, human_size(bytes)))
                .class("pd-btn pd-danger")
                .on_click(Msg::DeleteArm);
        }
    })
    .class("pd-deletebar");
}

/// (pro) Non-keeper files that carry an absolute path (native scans only) —
/// the actual delete list.
#[cfg(feature = "pro")]
fn deletable_files<'a>(
    view: &'a GroupsView,
    keepers: &Rc<RefCell<HashMap<u32, u32>>>,
) -> Vec<&'a PhotoRecord> {
    let by_id: HashMap<u32, &PhotoRecord> =
        view.records.iter().map(|r| (r.id, r)).collect();
    let mut out = Vec::new();
    for group in view.groups.iter() {
        let keeper = keeper_of(keepers, group);
        for id in &group.ids {
            if *id == keeper {
                continue;
            }
            if let Some(record) = by_id.get(id) {
                if record.absolute_path.is_some() {
                    out.push(*record);
                }
            }
        }
    }
    out
}

/// Free edition: the Pro upsell, nudging after the first completed scan.
#[cfg(not(feature = "pro"))]
fn upsell_view(scan_count: Signal<u32>) -> UITree<Msg> {
    let mut b = ContainerBuilder::<Msg>::new();
    if scan_count.get() == 0 {
        b.container(|_| {}).class("pd-upsell-hidden");
    } else {
        b.container(|c| {
            c.heading(3, "Cleaning up burst shots too?");
            c.text("The free edition only catches byte-identical copies. The Pro desktop edition ($19, one-time) adds:")
                .class("pd-hint");
            c.list(|items| {
                items.text("Perceptual near-duplicate detection — groups burst shots that aren't byte-identical").class("pd-upsell-item");
                items.text("Whole-drive scanning — no folder-by-folder hunting").class("pd-upsell-item");
                items.text("Side-by-side comparison so you can pick the keeper").class("pd-upsell-item");
                items.text("Batch delete of all non-keepers, with confirmation").class("pd-upsell-item");
            })
            .class("pd-upsell-list");
            c.link(
                "https://tptsolutions.co.nz/tools/photo-deduper",
                "Get the Pro desktop edition \u{2192}",
            )
            .class("pd-upsell-link")
            .attr("target", "_blank")
            .attr("rel", "noopener");
        })
        .class("pd-upsell");
    }
    b.into_only_child().expect("upsell view")
}

/// (pro) The side-by-side comparison overlay.
#[cfg(feature = "pro")]
fn compare_view(
    compare: Signal<Option<Rc<CompareState>>>,
    view: Rc<GroupsView>,
    thumbs: &Rc<RefCell<HashMap<u32, (u32, String)>>>,
    _keepers: &Rc<RefCell<HashMap<u32, u32>>>,
) -> UITree<Msg> {
    let mut b = ContainerBuilder::<Msg>::new();
    let by_id: HashMap<u32, &PhotoRecord> =
        view.records.iter().map(|r| (r.id, r)).collect();
    if let Some(state) = compare.get() {
        let candidates: Vec<u32> =
            state.ids.iter().copied().filter(|id| *id != state.keeper).collect();
        if let (Some(&candidate), Some(keeper_record), Some(candidate_record)) = (
            candidates.get(state.index),
            by_id.get(&state.keeper),
            candidates.get(state.index).and_then(|id| by_id.get(id)),
        ) {
            b.container(|overlay| {
                overlay.container(|panel| {
                    panel.container(|head| {
                        head.heading(3, "Side-by-side comparison");
                        head.button("Close").class("pd-btn").on_click(Msg::CompareClose);
                    }).class("pd-compare-head");
                    panel.container(|panes| {
                        pane(panes, thumbs, state.keeper, keeper_record, "Keeper", true);
                        pane(panes, thumbs, candidate, candidate_record, "Candidate", false);
                    })
                    .class("pd-compare-panes");
                    panel.container(|foot| {
                        foot.button("\u{2039} Previous")
                            .class("pd-btn pd-ghost")
                            .on_click(Msg::CompareNav(-1));
                        foot.text(format!(
                            "Candidate {} of {}",
                            state.index + 1,
                            candidates.len()
                        ))
                        .class("pd-compare-pos");
                        foot.button("Next \u{203a}")
                            .class("pd-btn pd-ghost")
                            .on_click(Msg::CompareNav(1));
                        foot.button("Make this the keeper")
                            .class("pd-btn pd-primary")
                            .on_click(Msg::SetKeeper { anchor: group_anchor(&state.ids), id: candidate });
                    })
                    .class("pd-compare-foot");
                })
                .class("pd-compare");
            })
            .class("pd-overlay")
            .on_click(Msg::CompareClose);
        }
    }
    b.into_only_child().expect("compare view")
}

/// Smallest id of a member list — the group anchor used for keeper state.
#[cfg(feature = "pro")]
fn group_anchor(ids: &[u32]) -> u32 {
    ids.iter().copied().min().unwrap_or(0)
}

#[cfg(feature = "pro")]
fn pane(
    c: &mut ContainerBuilder<Msg>,
    thumbs: &Rc<RefCell<HashMap<u32, (u32, String)>>>,
    id: u32,
    record: &PhotoRecord,
    role: &str,
    is_keeper: bool,
) {
    c.container(|p| {
        p.container(|role_row| {
            role_row
                .text(if is_keeper { format!("{role} \u{00b7} current keeper") } else { role.to_string() })
                .class(if is_keeper { "pd-pane-role pd-pane-keeper" } else { "pd-pane-role" });
        })
        .class("pd-pane-head");
        let existing = thumbs.borrow().get(&id).cloned();
        match existing {
            Some((_side, url)) => {
                p.image(url, format!("Full-size view of {}", record.name)).class("pd-pane-img");
            }
            None => {
                p.text("Loading preview\u{2026}").class("pd-hint");
            }
        }
        p.text(record.name.clone()).class("pd-name");
        p.text(format!("{} \u{00b7} {}", human_size(record.size), truncate(&record.path, 60)))
            .class("pd-path");
    })
    .class("pd-pane");
}

// ---------------------------------------------------------------------------
// Browser scan backend (free edition + pro-in-browser dev)
// ---------------------------------------------------------------------------

struct BrowserFile {
    file: web_sys::File,
    name: String,
    rel: String,
    size: u64,
}

#[allow(clippy::too_many_arguments)]
fn spawn_browser_scan(
    scan: Signal<ScanState>,
    scan_gen: Rc<std::cell::Cell<u32>>,
    thumbs: Rc<RefCell<HashMap<u32, (u32, String)>>>,
    object_urls: Rc<RefCell<Vec<String>>>,
    scan_count: Signal<u32>,
    #[cfg(feature = "pro")] near_on: Signal<bool>,
) {
    let Some(document) = web_sys::window().and_then(|window| window.document()) else {
        return;
    };
    let Ok(input) = document.create_element("input") else {
        return;
    };
    let _ = input.set_attribute("type", "file");
    let _ = input.set_attribute("webkitdirectory", "");
    let _ = input.set_attribute("multiple", "");
    let _ = input.set_attribute("style", "display:none");
    let Ok(input_el) = input.clone().dyn_into::<web_sys::HtmlInputElement>() else {
        return;
    };

    let scan_for_change = scan.clone();
    let scan_gen_for_change = scan_gen.clone();
    let thumbs_for_change = thumbs.clone();
    let object_urls_for_change = object_urls.clone();
    let scan_count_for_change = scan_count.clone();
    let on_change = wasm_bindgen::closure::Closure::<dyn FnMut(web_sys::Event)>::new(
        move |event: web_sys::Event| {
            let Some(target) = event.target() else { return };
            let Ok(input) = target.dyn_into::<web_sys::HtmlInputElement>() else { return };
            let Some(files) = input.files() else { return };

            let mut picked: Vec<BrowserFile> = Vec::new();
            let mut all_count = 0usize;
            for i in 0..files.length() {
                let Some(file) = files.item(i) else { continue };
                all_count += 1;
                let rel = js_sys::Reflect::get(&file, &JsValue::from_str("webkitRelativePath"))
                    .ok()
                    .and_then(|value| value.as_string())
                    .unwrap_or_default();
                let rel = if rel.is_empty() { file.name() } else { rel };
                if is_image_path(&rel) {
                    picked.push(BrowserFile {
                        size: file.size() as u64,
                        name: file.name(),
                        rel,
                        file,
                    });
                }
            }
            // Allow picking the same folder again later.
            input.set_value("");
            if picked.is_empty() {
                scan_for_change.set(ScanState::Failed(format!(
                    "No image files in that selection ({all_count} non-image file{} skipped).",
                    if all_count == 1 { "" } else { "s" }
                )));
                return;
            }

            // Fresh slate for the new scan.
            for url in object_urls_for_change.borrow_mut().drain(..) {
                let _ = web_sys::Url::revoke_object_url(&url);
            }
            thumbs_for_change.borrow_mut().clear();

            let skipped = all_count - picked.len();
            run_browser_scan(
                scan_for_change.clone(),
                scan_gen_for_change.clone(),
                thumbs_for_change.clone(),
                object_urls_for_change.clone(),
                scan_count_for_change.clone(),
                picked,
                skipped,
                #[cfg(feature = "pro")]
                near_on.clone(),
            );
        },
    );
    let _ = input.add_event_listener_with_callback(
        "change",
        on_change.as_ref().unchecked_ref(),
    );
    on_change.forget();
    if let Some(body) = document.body() {
        let _ = body.append_child(&input);
    }
    input_el.click();
    if let Some(body) = document.body() {
        let _ = body.remove_child(&input);
    }
}

#[allow(clippy::too_many_arguments)]
fn run_browser_scan(
    scan: Signal<ScanState>,
    scan_gen: Rc<std::cell::Cell<u32>>,
    thumbs: Rc<RefCell<HashMap<u32, (u32, String)>>>,
    object_urls: Rc<RefCell<Vec<String>>>,
    scan_count: Signal<u32>,
    files: Vec<BrowserFile>,
    skipped: usize,
    #[cfg(feature = "pro")] near_on: Signal<bool>,
) {
    let gen = scan_gen.get();
    let total = files.len();
    let folder = files
        .first()
        .map(|f| folder_of_rel(&f.rel))
        .unwrap_or_else(|| "(selection)".to_string());
    scan.set(ScanState::Scanning(Rc::new(ScanProgress {
        done: 0,
        total,
        current: String::new(),
        bytes_done: 0,
        skipped,
        native: false,
    })));

    wasm_bindgen_futures::spawn_local(async move {
        let mut records: Vec<PhotoRecord> = Vec::with_capacity(total);
        let mut thumb_files: Vec<(u32, web_sys::File)> = Vec::new();
        let mut bytes_done = 0u64;

        for file in files {
            if scan_gen.get() != gen {
                return; // superseded by a newer scan
            }
            let bytes = match JsFuture::from(file.file.array_buffer()).await {
                Ok(value) => js_sys::Uint8Array::new(&value).to_vec(),
                Err(_) => Vec::new(),
            };
            let hash = hash_bytes(&bytes);
            drop(bytes);

            #[cfg_attr(not(feature = "pro"), allow(unused_mut))]
            let mut perceptual = None;
            #[cfg(feature = "pro")]
            if near_on.get() && is_decodable_path(&file.rel) {
                perceptual = canvas_phash(&file.file).await;
            }

            let id = records.len() as u32;
            if is_decodable_path(&file.rel) {
                thumb_files.push((id, file.file.clone()));
            }
            records.push(PhotoRecord {
                id,
                name: file.name,
                path: file.rel,
                absolute_path: None,
                size: file.size,
                hash,
                perceptual,
            });
            bytes_done += file.size;
            scan.set(ScanState::Scanning(Rc::new(ScanProgress {
                done: records.len(),
                total,
                current: records.last().map(|r| r.path.clone()).unwrap_or_default(),
                bytes_done,
                skipped,
                native: false,
            })));
            // Macrotask yield so the browser paints between files.
            let _ = JsFuture::from(yield_now()).await;
        }

        // Thumbnails: object URLs point at the File blobs — no re-encode.
        {
            let mut cache = thumbs.borrow_mut();
            let mut urls = object_urls.borrow_mut();
            for (id, file) in &thumb_files {
                if let Ok(url) = web_sys::Url::create_object_url_with_blob(file.unchecked_ref()) {
                    cache.insert(*id, (THUMB_SIDE, url.clone()));
                    urls.push(url);
                }
            }
        }

        bump_scan_count(&scan_count);
        scan.set(ScanState::Done(Rc::new(ScanData {
            records,
            skipped,
            folder,
            native: false,
        })));
    });
}

/// Perceptual hash via the browser's own decoders: `createImageBitmap` →
/// 9x8 canvas → `getImageData` → BT.601 luma → dHash. `None` when the
/// format can't be decoded — the file still joins exact grouping.
#[cfg(feature = "pro")]
async fn canvas_phash(file: &web_sys::File) -> Option<u64> {
    let window = web_sys::window()?;
    let create: js_sys::Function = js_sys::Reflect::get(&window, &JsValue::from_str("createImageBitmap"))
        .ok()?
        .dyn_into()
        .ok()?;
    let promise = create
        .call1(&window, file.unchecked_ref::<web_sys::Blob>())
        .ok()?
        .dyn_into::<js_sys::Promise>()
        .ok()?;
    let bitmap_value = JsFuture::from(promise).await.ok()?;
    let bitmap: web_sys::ImageBitmap = bitmap_value.dyn_into().ok()?;

    let document = window.document()?;
    let canvas: web_sys::HtmlCanvasElement = document
        .create_element("canvas")
        .ok()?
        .dyn_into()
        .ok()?;
    canvas.set_width(9);
    canvas.set_height(8);
    let result = (|| {
        let ctx_value = canvas.get_context("2d").ok()??;
        let ctx: web_sys::CanvasRenderingContext2d = ctx_value.dyn_into().ok()?;
        // No drawImage(w, h) overload on this web-sys: scale instead, so
        // the bitmap's full frame lands in the 9x8 canvas.
        let (bw, bh) = (bitmap.width().max(1) as f64, bitmap.height().max(1) as f64);
        let _ = ctx.set_transform(9.0 / bw, 0.0, 0.0, 8.0 / bh, 0.0, 0.0);
        ctx.draw_image_with_image_bitmap(&bitmap, 0.0, 0.0).ok()?;
        let data = ctx
            .get_image_data(0.0, 0.0, 9.0, 8.0)
            .ok()?
            .data()
            .to_vec();
        tpt_photo_dedupe::dhash_from_gray(&rgba_to_gray(&data))
    })();
    bitmap.close();
    result
}

// ---------------------------------------------------------------------------
// Native scan backend (Pro desktop shell, via the __appfront bridge)
// ---------------------------------------------------------------------------

#[cfg(feature = "pro")]
fn spawn_native_scan(
    scan: Signal<ScanState>,
    scan_gen: Rc<std::cell::Cell<u32>>,
    thumbs: Rc<RefCell<HashMap<u32, (u32, String)>>>,
    thumbs_version: Signal<u32>,
    scan_count: Signal<u32>,
    near: bool,
) {
    wasm_bindgen_futures::spawn_local(async move {
        let gen = scan_gen.get();
        let Some(picked) = ipc_call(
            "dialog.open",
            serde_json::json!({"dir": true, "title": "Choose a folder or drive to scan"}),
        )
        .await
        else {
            scan.set(ScanState::Idle);
            return;
        };
        let Some(dir) = picked.get("path").and_then(|value| value.as_str()) else {
            scan.set(ScanState::Idle); // user cancelled the folder dialog
            return;
        };
        let dir = dir.to_string();
        let folder = dir
            .rsplit(['/', '\\'])
            .next()
            .filter(|name| !name.is_empty())
            .unwrap_or(&dir)
            .to_string();

        thumbs.borrow_mut().clear();
        scan.set(ScanState::Scanning(Rc::new(ScanProgress {
            done: 0,
            total: 0,
            current: format!("Walking {dir}"),
            bytes_done: 0,
            skipped: 0,
            native: true,
        })));

        let Some(start) = ipc_call(
            "scan_start",
            serde_json::json!({"dir": dir, "near": near}),
        )
        .await
        else {
            scan.set(ScanState::Failed("The desktop bridge rejected scan_start.".to_string()));
            return;
        };
        let Some(native_id) = start.get("scan_id").and_then(|value| value.as_u64()) else {
            scan.set(ScanState::Failed("The desktop bridge returned no scan id.".to_string()));
            return;
        };

        // Poll progress every 250 ms until the native walk finishes.
        loop {
            let _ = JsFuture::from(yield_now_ms(250)).await;
            if scan_gen.get() != gen {
                let _ = ipc_call("scan_cancel", serde_json::json!({"scan_id": native_id})).await;
                return;
            }
            let Some(progress) = ipc_call(
                "scan_progress",
                serde_json::json!({"scan_id": native_id}),
            )
            .await
            else {
                continue;
            };
            let done = progress.get("done").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
            let total = progress.get("total").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
            let current = progress
                .get("current")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            let failed = progress
                .get("error")
                .and_then(|v| v.as_str())
                .map(str::to_string);
            scan.set(ScanState::Scanning(Rc::new(ScanProgress {
                done,
                total,
                current,
                bytes_done: progress.get("bytes_done").and_then(|v| v.as_u64()).unwrap_or(0),
                skipped: progress.get("skipped").and_then(|v| v.as_u64()).unwrap_or(0) as usize,
                native: true,
            })));
            if let Some(error) = failed {
                scan.set(ScanState::Failed(error));
                return;
            }
            if progress.get("finished").and_then(|v| v.as_bool()).unwrap_or(false) {
                break;
            }
        }

        let Some(results) = ipc_call(
            "scan_results",
            serde_json::json!({"scan_id": native_id}),
        )
        .await
        else {
            scan.set(ScanState::Failed("The desktop bridge rejected scan_results.".to_string()));
            return;
        };
        let mut records: Vec<PhotoRecord> = Vec::new();
        if let Some(entries) = results.get("records").and_then(|value| value.as_array()) {
            for entry in entries {
                let Some(path) = entry.get("path").and_then(|value| value.as_str()) else {
                    continue;
                };
                let mut hash = [0u8; 32];
                if let Some(hex) = entry.get("hash").and_then(|value| value.as_str()) {
                    for (i, byte) in hash.iter_mut().enumerate() {
                        if let Ok(value) =
                            u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16)
                        {
                            *byte = value;
                        }
                    }
                }
                let name = path
                    .rsplit(['/', '\\'])
                    .next()
                    .unwrap_or(path)
                    .to_string();
                records.push(PhotoRecord {
                    id: records.len() as u32,
                    name,
                    path: path.to_string(),
                    absolute_path: Some(path.to_string()),
                    size: entry.get("size").and_then(|value| value.as_u64()).unwrap_or(0),
                    hash,
                    perceptual: entry.get("phash").and_then(|value| value.as_u64()),
                });
            }
        }
        if records.is_empty() {
            scan.set(ScanState::Failed(format!(
                "No image files found under {folder}."
            )));
            return;
        }

        bump_scan_count(&scan_count);
        scan.set(ScanState::Done(Rc::new(ScanData {
            skipped: 0,
            folder,
            native: true,
            records: records.clone(),
        })));

        // Thumbnails stream in chunks; each landed chunk bumps the version
        // signal, which re-renders the results grid.
        fetch_native_thumbs(&records, &thumbs, THUMB_SIDE, thumbs_version).await;
    });
}

/// Fetches native data-URL thumbnails for `ids`/`records` at `side` px in
/// chunks, inserting into the shared cache. Callers bump the version signal
/// to re-render (the fetch loop here bumps per chunk).
#[cfg(feature = "pro")]
async fn fetch_native_thumbs(
    records: &[PhotoRecord],
    thumbs: &Rc<RefCell<HashMap<u32, (u32, String)>>>,
    side: u32,
    version: Signal<u32>,
) {
    for chunk in records.chunks(40) {
        let paths: Vec<String> = chunk.iter().map(|r| r.path.clone()).collect();
        let missing: Vec<&PhotoRecord> = chunk
            .iter()
            .filter(|r| !thumbs.borrow().contains_key(&r.id))
            .collect();
        if missing.is_empty() {
            continue;
        }
        if let Some(reply) = ipc_call(
            "thumbnails",
            serde_json::json!({"paths": paths, "side": side}),
        )
        .await
        {
            let map = reply.get("thumbs").and_then(|value| value.as_object()).cloned();
            if let Some(map) = map {
                let mut cache = thumbs.borrow_mut();
                for record in missing {
                    if let Some(url) = map.get(&record.path).and_then(|value| value.as_str()) {
                        cache.insert(record.id, (side, url.to_string()));
                    } else {
                        // Mark undecodable files so we don't re-ask.
                        cache.insert(record.id, (side, String::new()));
                    }
                }
            }
            bump(&version);
        }
    }
}

/// Ensures thumbnails exist at (at least) `side` for the given records —
/// used by the compare overlay, which wants bigger images than the grid.
#[cfg(feature = "pro")]
async fn ensure_thumbs_for(
    view: &GroupsView,
    thumbs: &Rc<RefCell<HashMap<u32, (u32, String)>>>,
    side: u32,
    ids: &[u32],
) {
    let needed: Vec<PhotoRecord> = ids
        .iter()
        .filter_map(|id| {
            let record = view.records.iter().find(|r| r.id == *id)?;
            let cached = thumbs.borrow().get(id).map(|(cached_side, url)| {
                *cached_side >= side && !url.is_empty()
            });
            if cached.unwrap_or(false) {
                None
            } else {
                Some(record.clone())
            }
        })
        .collect();
    if needed.is_empty() {
        return;
    }
    if let Some(reply) = ipc_call(
        "thumbnails",
        serde_json::json!({"paths": needed.iter().map(|r| r.path.clone()).collect::<Vec<_>>(), "side": side}),
    )
    .await
    {
        let map = reply.get("thumbs").and_then(|value| value.as_object()).cloned();
        if let Some(map) = map {
            let mut cache = thumbs.borrow_mut();
            for record in &needed {
                if let Some(url) = map.get(&record.path).and_then(|value| value.as_str()) {
                    cache.insert(record.id, (side, url.to_string()));
                }
            }
        }
    }
}

/// (pro, desktop) Deletes every non-keeper with an absolute path, then
/// folds the outcome back into the scan state (deleted files vanish).
#[cfg(feature = "pro")]
fn spawn_delete(
    scan: Signal<ScanState>,
    groups: Signal<Rc<GroupsView>>,
    keepers: Rc<RefCell<HashMap<u32, u32>>>,
    delete_error: Signal<Option<String>>,
) {
    let view = groups.get();
    let targets = deletable_files(&view, &keepers);
    if targets.is_empty() {
        delete_error.set(Some(
            "Nothing can be deleted from this scan — the browser edition has no file access."
                .to_string(),
        ));
        return;
    }
    let paths: Vec<String> = targets
        .iter()
        .filter_map(|record| record.absolute_path.clone())
        .collect();

    wasm_bindgen_futures::spawn_local(async move {
        let Some(reply) = ipc_call("delete_files", serde_json::json!({"paths": paths.clone()})).await
        else {
            delete_error.set(Some("The desktop bridge rejected the delete command.".to_string()));
            return;
        };
        let deleted = reply.get("deleted").and_then(|value| value.as_u64()).unwrap_or(0);
        let failed: Vec<String> = reply
            .get("failed")
            .and_then(|value| value.as_array())
            .map(|entries| {
                entries
                    .iter()
                    .filter_map(|entry| entry.get("path").and_then(|value| value.as_str()))
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();

        if deleted == 0 && !failed.is_empty() {
            delete_error.set(Some(format!("Delete failed: {}", failed[0])));
            return;
        }
        let failed_set: std::collections::HashSet<&str> =
            failed.iter().map(String::as_str).collect();
        if let ScanState::Done(data) = scan.get() {
            let records: Vec<PhotoRecord> = data
                .records
                .iter()
                .filter(|record| match &record.absolute_path {
                    Some(path) => !failed_set.contains(path.as_str()),
                    None => true,
                })
                .cloned()
                .collect();
            scan.set(ScanState::Done(Rc::new(ScanData {
                skipped: data.skipped,
                folder: data.folder.clone(),
                native: data.native,
                records,
            })));
            // Stale keepers vanish with their groups.
            keepers.borrow_mut().retain(|_, _| true);
        }
        if !failed.is_empty() {
            delete_error.set(Some(format!(
                "{} file{} could not be deleted (in use or moved).",
                failed.len(),
                if failed.len() == 1 { "" } else { "s" }
            )));
        }
    });
}

// ---------------------------------------------------------------------------
// Bridge plumbing
// ---------------------------------------------------------------------------

/// Whether the desktop shell injected its IPC bridge — i.e. absolute paths,
/// native scanning, thumbnails and deletion are available.
fn has_native_bridge() -> bool {
    web_sys::window()
        .map(|window| {
            js_sys::Reflect::has(&window, &JsValue::from_str("__appfront")).unwrap_or(false)
        })
        .unwrap_or(false)
}

/// One ACL-gated IPC round trip: `__appfront.post(action, params, requestId)`
/// → `__appfrontResolve(requestId, reply)`. Replies parse back as
/// `serde_json::Value`; `None` means the bridge is absent or the call failed.
#[cfg(feature = "pro")]
async fn ipc_call(action: &str, params: serde_json::Value) -> Option<serde_json::Value> {
    use std::sync::atomic::{AtomicU32, Ordering};
    static REQUEST_ID: AtomicU32 = AtomicU32::new(1);

    let window = web_sys::window()?;
    let bridge = js_sys::Reflect::get(&window, &JsValue::from_str("__appfront")).ok()?;
    let post: js_sys::Function = js_sys::Reflect::get(&bridge, &JsValue::from_str("post"))
        .ok()?
        .dyn_into()
        .ok()?;
    let params_js = js_sys::JSON::parse(&params.to_string()).ok()?;
    let request_id = format!("dedupe-{}", REQUEST_ID.fetch_add(1, Ordering::Relaxed));
    let reply = JsFuture::from(
        post.call3(
            &bridge,
            &JsValue::from_str(action),
            &params_js,
            &JsValue::from_str(&request_id),
        )
        .ok()?
        .dyn_into::<js_sys::Promise>()
        .ok()?,
    )
    .await
    .ok()?;
    if reply.is_undefined() || reply.is_null() {
        return Some(serde_json::Value::Null);
    }
    let text = js_sys::JSON::stringify(&reply).ok()?;
    let text: String = text.into();
    serde_json::from_str(&text).ok()
}

// ---------------------------------------------------------------------------
// Small utilities
// ---------------------------------------------------------------------------

#[cfg(feature = "pro")]
fn bump(signal: &Signal<u32>) {
    signal.set(signal.get() + 1);
}

fn bump_scan_count(signal: &Signal<u32>) {
    let next = signal.get() + 1;
    storage_set("dedupe-scan-count", &next.to_string());
    signal.set(next);
}

fn folder_of_rel(rel: &str) -> String {
    rel.split(['/', '\\']).next().unwrap_or_default().to_string()
}

fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.to_string()
    } else {
        let cut: String = text.chars().take(max.saturating_sub(1)).collect();
        format!("{cut}\u{2026}")
    }
}

fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

fn storage_get(key: &str) -> Option<String> {
    web_sys::window()?
        .local_storage()
        .ok()
        .flatten()?
        .get_item(key)
        .ok()
        .flatten()
}

fn storage_set(key: &str, value: &str) {
    if let Some(storage) = web_sys::window().and_then(|window| window.local_storage().ok().flatten())
    {
        let _ = storage.set_item(key, value);
    }
}

fn upsert_style(document: &web_sys::Document, id: &str, css: &str) {
    if document.get_element_by_id(id).is_some() {
        return;
    }
    if let Ok(style) = document.create_element("style") {
        let _ = style.set_attribute("id", id);
        let _ = style.set_attribute("data-tpt-style", id);
        let _ = style.set_text_content(Some(css));
        if let Some(head) = document.head() {
            let _ = head.append_child(&style);
        }
    }
}

fn yield_now() -> js_sys::Promise {
    yield_now_ms(0)
}

fn yield_now_ms(ms: i32) -> js_sys::Promise {
    js_sys::Promise::new(&mut |resolve: js_sys::Function, _reject: js_sys::Function| {
        if let Some(window) = web_sys::window() {
            let _ = window.set_timeout_with_callback_and_timeout_and_arguments_0(
                &resolve,
                ms.max(0),
            );
        }
    })
}

// ---------------------------------------------------------------------------
// Styles
// ---------------------------------------------------------------------------

const BASE_CSS: &str = r#"
.pd-app{--pd-bg:#ffffff;--pd-panel:#f6f7f9;--pd-fg:#1f2430;--pd-muted:#667085;--pd-border:#d9dee7;--pd-accent:#0e7c6b;--pd-accent-fg:#ffffff;--pd-err:#b3261e;--pd-err-bg:#fbeeec;--pd-keep:#e7f5f1;--pd-keep-border:#0e7c6b;max-width:960px;color:var(--pd-fg);font-size:15px;line-height:1.5}
@media(prefers-color-scheme:dark){.pd-app{--pd-bg:#10151f;--pd-panel:#161d2a;--pd-fg:#e6e9ef;--pd-muted:#98a2b3;--pd-border:#2a3342;--pd-accent:#34b39a;--pd-accent-fg:#0b1f1a;--pd-err:#f2b8b5;--pd-err-bg:#2c1a1c;--pd-keep:#12271f;--pd-keep-border:#34b39a}}
.pd-app *{box-sizing:border-box}
.pd-app h2{font-size:1.3rem;margin:0 0 2px}
.pd-app h3{font-size:1rem;margin:0 0 6px}
.pd-sub{color:var(--pd-muted);margin:0 0 14px;font-size:.9rem}
.pd-header{display:flex;justify-content:space-between;align-items:baseline;gap:10px;flex-wrap:wrap}
.pd-badge{font-size:.72rem;font-weight:700;letter-spacing:.04em;text-transform:uppercase;color:var(--pd-muted);border:1px solid var(--pd-border);border-radius:999px;padding:2px 10px}
.pd-card{background:var(--pd-panel);border:1px solid var(--pd-border);border-radius:10px;padding:14px 16px;margin:0 0 14px}
.pd-hint{color:var(--pd-muted);font-size:.82rem;margin:0 0 8px}
.pd-actions{display:flex;gap:12px;flex-wrap:wrap;margin-top:10px;align-items:center}
.pd-btn{padding:8px 16px;border-radius:8px;border:1px solid var(--pd-border);background:var(--pd-bg);color:var(--pd-fg);font:inherit;cursor:pointer}
.pd-btn:hover{border-color:var(--pd-accent)}
.pd-primary{background:var(--pd-accent);border-color:var(--pd-accent);color:var(--pd-accent-fg);font-weight:600}
.pd-ghost{background:transparent}
.pd-danger{background:var(--pd-err);border-color:var(--pd-err);color:#fff;font-weight:600}
.pd-field{display:flex;flex-direction:column;gap:4px}
.pd-label{font-size:.78rem;color:var(--pd-muted)}
.pd-app select{padding:6px 8px;border:1px solid var(--pd-border);border-radius:8px;background:var(--pd-bg);color:var(--pd-fg);font:inherit}
.pd-near-toggle{display:flex;align-items:center;gap:6px;font-size:.88rem}
.pd-progress-head{display:flex;justify-content:space-between;align-items:center;gap:10px}
.pd-bar{height:8px;border-radius:5px;background:color-mix(in srgb,var(--pd-border) 55%,transparent);overflow:hidden;margin:10px 0 6px}
.pd-bar-fill{height:100%;background:var(--pd-accent);border-radius:5px;transition:width .15s ease}
.pd-current{font-family:ui-monospace,SFMono-Regular,Menlo,Consolas,monospace;font-size:.78rem;color:var(--pd-muted);margin:0}
.pd-error{color:var(--pd-err);background:var(--pd-err-bg);border:1px solid var(--pd-err);border-radius:8px;padding:8px 12px;font-size:.88rem;margin:8px 0}
.pd-empty{color:var(--pd-muted);margin:0}
.pd-headline{font-weight:700;font-size:1.02rem;margin:0 0 2px}
.pd-folder{color:var(--pd-muted);font-size:.85rem;margin:0 0 10px;font-family:ui-monospace,SFMono-Regular,Menlo,Consolas,monospace}
.pd-stats{display:grid;grid-template-columns:repeat(auto-fit,minmax(140px,1fr));gap:8px;margin:0 0 12px}
.pd-stat{background:var(--pd-bg);border:1px solid var(--pd-border);border-radius:8px;padding:8px 10px}
.pd-stat-value{font-weight:700;font-size:1.15rem;margin:0}
.pd-stat-label{color:var(--pd-muted);font-size:.75rem;margin:0}
.pd-deletebar{display:flex;gap:10px;align-items:center;flex-wrap:wrap;border:1px solid var(--pd-border);border-radius:10px;padding:10px 12px;margin:0 0 12px;background:var(--pd-bg)}
.pd-delete-note{margin:0}
.pd-arm-warning{font-weight:600;margin:0}
.pd-group{border:1px solid var(--pd-border);border-radius:10px;padding:10px 12px;margin:0 0 12px;background:var(--pd-bg)}
.pd-group-top{display:flex;justify-content:space-between;align-items:center;gap:10px;flex-wrap:wrap;margin-bottom:8px}
.pd-group-head{display:flex;gap:10px;align-items:baseline;flex-wrap:wrap}
.pd-badge-identical{color:var(--pd-accent);border-color:var(--pd-accent)}
.pd-badge-near{color:#b06e0a;border-color:#b06e0a}
@media(prefers-color-scheme:dark){.pd-badge-near{color:#e5a53d;border-color:#e5a53d}}
.pd-group-meta{color:var(--pd-muted);font-size:.85rem}
.pd-row{display:flex;gap:12px;align-items:center;border:1px solid var(--pd-border);border-radius:8px;padding:8px;margin:0 0 6px}
.pd-row-keeper{background:var(--pd-keep);border-color:var(--pd-keep-border)}
.pd-thumb{width:88px;height:88px;object-fit:cover;border-radius:6px;flex:none;border:1px solid var(--pd-border)}
.pd-thumb-empty{display:flex;align-items:center;justify-content:center;background:var(--pd-panel);color:var(--pd-muted);font-size:.6rem}
.pd-fileinfo{flex:1;min-width:0;display:flex;flex-direction:column}
.pd-name{font-weight:600;font-size:.9rem;overflow:hidden;text-overflow:ellipsis;white-space:nowrap}
.pd-path{color:var(--pd-muted);font-size:.78rem;overflow:hidden;text-overflow:ellipsis;white-space:nowrap;font-family:ui-monospace,SFMono-Regular,Menlo,Consolas,monospace}
.pd-size{color:var(--pd-muted);font-size:.78rem}
.pd-keeper-tag{font-weight:700;color:var(--pd-accent);font-size:.85rem;flex:none}
.pd-upsell-hidden{margin:0;padding:0}
.pd-upsell{border:1px dashed var(--pd-border);border-radius:10px;padding:12px 16px;color:var(--pd-muted);font-size:.88rem;margin-top:4px}
.pd-upsell h3{color:var(--pd-fg)}
.pd-upsell-list{margin:0 0 10px;padding-left:18px}
.pd-upsell-item{margin:2px 0;list-style:disc}
.pd-upsell-link{color:var(--pd-accent);font-weight:600}
.pd-overlay{position:fixed;inset:0;background:rgba(8,12,20,.62);display:flex;align-items:center;justify-content:center;z-index:60;padding:20px}
.pd-compare{background:var(--pd-panel);border:1px solid var(--pd-border);border-radius:12px;padding:16px;max-width:94vw;max-height:94vh;overflow:auto}
.pd-compare-head{display:flex;justify-content:space-between;align-items:center;gap:12px;margin-bottom:10px}
.pd-compare-panes{display:flex;gap:12px;flex-wrap:wrap}
.pd-pane{flex:1 1 320px;min-width:280px;border:1px solid var(--pd-border);border-radius:10px;padding:10px;background:var(--pd-bg)}
.pd-pane-head{margin-bottom:6px}
.pd-pane-role{font-size:.75rem;font-weight:700;letter-spacing:.05em;text-transform:uppercase;color:var(--pd-muted)}
.pd-pane-keeper{color:var(--pd-accent)}
.pd-pane-img{max-width:100%;max-height:64vh;object-fit:contain;border-radius:8px;display:block;margin:0 auto 6px}
.pd-compare-foot{display:flex;gap:10px;align-items:center;flex-wrap:wrap;margin-top:10px}
.pd-compare-pos{color:var(--pd-muted);font-size:.85rem;margin:0 auto}
"#;

