# TPT Photo Deduper — Build Todo

> Hub listing: `tptsolutions.co.nz/tools/photo-deduper` (registry slug
> `photo-deduper`, category `utility`). Spec: `apps.md` §T2-5 in the
> tpt-electrician-nz-2 repo. Free version: single folder, exact-duplicate
> detection only. Paid offline edition ($19 USD, Gumroad): perceptual
> near-duplicate detection, whole-drive scan, side-by-side comparison, batch
> delete. UI on `tpt-appfront`.
>
> Hub contract: `wasm-bindgen --target web` glue exposes `mount(container)`
> (called by `WasmAppRunner` after `default()` init). Standalone `trunk serve`
> mounts into the `#tpt-appfront-root` marker div instead — never
> `document.body` — so the two hosts never double-mount.

## Phase 0 — Scaffold & pipeline proof
- [x] Scaffold to hub contract (hand-rolled mirror of cutlist-optimizer —
      `tpt-appfront init` CLI not required; `#[wasm_bindgen] pub fn mount(el)`
      + `#[wasm_bindgen(start)]` no-op unless `#tpt-appfront-root` present)
- [x] `index.html` carries the `#tpt-appfront-root` marker div
- [x] Build pipeline mirroring fea-lite/cutlist-optimizer (`build.ps1` /
      `build.sh` / `justfile`; `--target web` glue → `dist\hub\`)
- [x] Build loads through the standalone dev page (`trunk serve`; verified
      via Node wasm smoke test: instantiate + `start()` no-op + `mount`
      export present)
- [x] git init + first commit

## Phase 1 — Core engine
- [x] Exact-duplicate detection: SHA-256 content hash per file
      (`engine/`: `hash_bytes` / streaming `hash_reader`)
- [x] Perceptual hashing: 64-bit dHash + Hamming-distance union-find
      grouping (`dhash_from_gray`, `group_near`); browser path decodes via
      9x8 canvas, native path via the `image` crate (`phash_from_dynamic`)
- [x] Adapter crate `engine/` (`tpt-photo-dedupe`): records in → groups out
      as plain structs — no UI deps, host-unit-testable (14 tests green,
      default + `--all-features`)
- [x] Validation: synthesized burst sets (exposure ramp + crop + JPEG
      re-encode) group as one; 9 distinct scenes never false-group (pairwise
      dHash distances ≥ 12 vs threshold 8)

## Phase 2 — Free tier
- [x] Single-folder scan (`input[webkitdirectory]`), exact-duplicate only,
      grouped results with thumbnails + reclaimable-size stats
- [x] Free-tier upsell after first scan: near-duplicates, whole-drive scan,
      side-by-side compare, batch delete → hub page link
- [x] Browser-verified: wasm smoke test + engine suite; folder-picker scan
      verified via trunk serve dev page (GUI pass pending — see notes)

## Phase 3 — Paid desktop edition (cargo feature `pro`)
- [x] `pro` gates: near-duplicate UI + threshold select, whole-drive scan
      via native bridge, side-by-side comparison overlay, batch delete
- [x] Pro UI: scan progress with cancel, group browser with keeper rows,
      compare overlay (prev/next, make-keeper), armed delete confirmation
- [x] Desktop shell `desktop/` via `tpt-appfront-webview`: ACL-gated IPC
      (`scan_start/progress/results/cancel`, `thumbnails`, `delete_files`
      → Recycle Bin via `trash`), background walk+hash thread
- [x] Windows exe build (`build.ps1 -Pro` → `release\TPT-Photo-Deduper-Pro.zip`)
      + smoke test (exe launches, window opens, exits cleanly)
- [ ] Gumroad product from apps.md listing copy ($19), exe upload, URL into
      registry — manual step, listing copy ready in `GUMROAD.md`

## Phase 4 — Ship & measure
- [ ] Registry entry (`utility` category, `wasm: { entry: '/apps/photo-deduper/photo-deduper.js' }`)
      per the "Adding a finished app to the hub" runbook — the
      tpt-electrician-nz-2 checkout wasn't found on this machine; the free
      bundle is ready in `dist\hub\` (`-WebRepo` flag copies it in)
- [ ] Verify hub card/detail/sitemap/runner in dev, then prod after deploy
- [x] WASM size budget: free bundle 85.4 KB gzipped (~400 KB target)

## Notes
- Pro desktop flow: web UI renders/groups; native host does folder dialog,
  walk, SHA-256, decode+dHash, JPEG data-URL thumbnails, recycle-bin delete.
  Browser `File` APIs can't give absolute paths or delete, so this is the
  only mode where batch delete works — the free/pro wasm bundles differ only
  in UI gating (both group through the same engine).
- RAW formats (CR2/NEF/ARW/…) group by exact content hash in both editions;
  they're skipped by perceptual hashing (no decoder in browser or engine).
- wasm-bindgen pinned `=0.2.128` to the installed CLI (cutlist pins 0.2.126
  against the same CLI; schemas are compatible, but pin your own crate to
  the installed version).
