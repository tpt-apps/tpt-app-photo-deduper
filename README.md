# TPT Photo Deduper

Finds duplicate photos. Free edition: byte-identical copies in one folder,
right in your browser. Pro edition ($19, Windows): perceptual near-duplicate
detection for burst shots, whole-drive scanning, side-by-side comparison, and
batch delete — an offline desktop app.

Part of the [TPT AppFront](../tpt-appfront) app suite; hub listing lives at
`tptsolutions.co.nz/tools/photo-deduper` (registry slug `photo-deduper`,
category `utility`).

## Layout

```
engine/            tpt-photo-dedupe — the moat. SHA-256 exact grouping,
                   dHash + Hamming near grouping, zero UI deps, host-tested.
src/               tpt-app-photo-deduper — the appfront DOM app (wasm32).
desktop/           tpt-photo-deduper-desktop — native webview shell (Pro):
                   folder dialog, background walk+hash, thumbnails,
                   recycle-bin delete over the ACL-gated IPC bridge.
index.html         trunk dev shell (#tpt-appfront-root marker div).
build.ps1|sh       pipeline: free wasm -> dist\hub, pro -> dist\desktop + exe.
justfile           thin wrappers (build/pro/deploy/test/check/dev/dev-pro).
GUMROAD.md         everything needed to publish the Pro listing.
```

## Hub contract

`wasm-bindgen --target web` glue exposing `mount(container)` — called by the
hub's `WasmAppRunner` after awaiting the default export. Standalone
`trunk serve` mounts into the `#tpt-appfront-root` marker div instead, so the
two hosts never double-mount. The website always gets the FREE bundle
(`dist/hub/`); the Pro bundle only exists inside the desktop package.

## Development

```sh
just dev            # trunk serve, free edition
just dev-pro        # trunk serve, Pro UI (browser scan path, no bridge)
just test           # engine suite, default + all features, app unit tests
just check          # wasm free + wasm pro + desktop shell
just build          # free wasm bundle -> dist\hub (with gzip size report)
just pro            # Pro trunk bundle + exe + release\ zip package
```

`TPT_WEB_REPO=<web repo checkout> just deploy` copies the free bundle into
`public/apps/photo-deduper/`; the registry entry then flips `status` to
`'live'` with `wasm: { entry: '/apps/photo-deduper/photo-deduper.js' }`.

## How scanning works

- **Free (browser)**: hidden `input[webkitdirectory]` → per-file
  `arrayBuffer()` → SHA-256 (engine) → group exact duplicates. Files are
  yielded to the event loop one at a time with a progress bar.
- **Pro (browser dev)**: same path, plus near-duplicate detection —
  `createImageBitmap` → 9x8 canvas → BT.601 luma → dHash (engine) — grouped
  by Hamming distance. Delete stays desktop-only.
- **Pro (desktop)**: the `window.__appfront` bridge hands the whole scan to
  the native host: folder dialog, recursive walk (system dirs skipped),
  streaming SHA-256, `image`-crate decode for dHash, JPEG data-URL
  thumbnails, and `trash`-crate (Recycle Bin) delete. The UI and grouping
  logic are byte-for-byte the same code as the browser path.

Perceptual grouping: union-find merges identical content hashes first, then
dHash neighbours within the threshold (Strict 6 / Balanced 8 / Loose 14;
default 8). RAW formats (CR2/NEF/ARW/…) always participate in exact
grouping; they're skipped by perceptual hashing because neither the browser
nor this crate decodes them.
