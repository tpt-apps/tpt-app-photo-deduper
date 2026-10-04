# Gumroad listing

Everything needed to create/update the Gumroad product once this app is
built. Source copy is `apps.md` §T2-5 in the `tpt-electrician-nz-2` repo — if
that changes, update here too (and vice versa).

## Step-by-step: publish this listing

1. Build the Pro edition (`./build.ps1 -Pro`, once the build pipeline exists
   — see `todo.md` and mirror `tpt-app-fea-lite`/`tpt-app-cutlist-optimizer`'s
   pattern) — produces the packaged exe/zip in `release/`.
2. Log in to Gumroad → **New product** → **Digital product**.
3. Set the product name, price, and upload the packaged file (all below).
4. Paste the short description, bullets, and full description (below) into
   their respective fields.
5. Set category/tags (below) and add a cover image (see note below).
6. Publish, then copy the resulting product URL (`https://gum.co/...`).
7. Paste that URL into the `photo-deduper` entry's `gumroadUrl` field in
   `tpt-electrician-nz-2\src\lib\tpt-apps-registry.ts`, and set `price: '$19'`
   on the same entry.
8. Flip that entry's `status` to `'live'` once both the free demo and this
   Gumroad listing exist.
9. Commit and push the registry repo.

## File to upload

Not built yet. Once packaged, this will be the Pro exe/zip produced by the
app's `build.ps1 -Pro` (or equivalent) — see `todo.md`.

## Product name

```
TPT Photo Deduper — Find Near-Duplicate Burst Shots, Not Just Exact Copies
```

## Price

**$19 USD, one-time purchase** (no subscription).

## Short description / summary field

```
Free dupe finders only catch identical files. This finds visually-similar burst shots and lets you pick the keeper in seconds.
```

## Bullets / feature list

```
Perceptual near-duplicate detection
Burst grouping
Whole-drive scan
Batch cleanup
One-time purchase
```

## Full description (long-form field)

```
TPT Photo Deduper uses perceptual image hashing to find visually-similar
(not just byte-identical) photos, groups burst sequences together, and lets
you pick the keeper from each group.

Who it's for: photographers and videographers with huge RAW/JPEG libraries
full of near-identical burst-mode shots — the actual task photographers do
manually in culling sessions today, since most free duplicate-finders only
catch byte-identical files.

Pro edition (this download):
- Perceptual near-duplicate detection (free is exact-duplicate only)
- Whole-drive scanning
- Side-by-side burst comparison view
- Batch-delete of non-keepers

Try the free exact-duplicate edition first, right in your browser, no
install: tptsolutions.co.nz/tools/photo-deduper

Requirements: Windows 10 (20H2+) or Windows 11. Uses the WebView2 runtime,
preinstalled on both — nothing extra to download.
```

## Category / tags

- Category: **Software > Utility** (or Gumroad's closest equivalent)
- Tags: `photo-management`, `duplicate-finder`, `photography-tools`,
  `raw-photo`, `culling`, `windows`

## Cover image / thumbnail

Not created yet. A screenshot of a burst-sequence group with the keeper
highlighted is the most convincing cover once built. No stock/AI art.

## After publishing

1. Copy the resulting product URL (`https://gum.co/...`).
2. Paste it into the `gumroadUrl` field of the `photo-deduper` entry in
   `tpt-electrician-nz-2\src\lib\tpt-apps-registry.ts`, and set `price: '$19'`.
3. Set `status` to `'live'`.
4. Commit and push the registry repo.
