#!/usr/bin/env sh
# TPT Photo Deduper - build pipeline (macOS/Linux).
#
# Outputs (separate directories - they never overwrite each other):
#   dist/hub/      free wasm bundle for the website
#   dist/desktop/  trunk bundle the Pro exe serves
#   release/       staged exe package (-Pro)
#
# Website upload:
#   ./build.sh                     (free bundle -> dist/hub)
#   TPT_WEB_REPO=/path/to/web-repo ./build.sh   (also copies it in)
#   then set the app's registry entry in the web repo:
#     status: 'live'
#     wasm: { entry: '/apps/photo-deduper/photo-deduper.js' }
#
# Pro edition:  ./build.sh --pro

set -eu
cd "$(dirname "$0")"

CRATE=tpt-app-photo-deduper
TARGET=wasm32-unknown-unknown
OUT_NAME=photo-deduper

PRO=0
for arg in "$@"; do
  case "$arg" in
    --pro) PRO=1 ;;
    *) echo "usage: build.sh [--pro]" >&2; exit 2 ;;
  esac
done

if [ -n "${TPT_WEB_REPO:-}" ]; then
  WEB_DEST="$TPT_WEB_REPO/public/apps/photo-deduper"
else
  WEB_DEST=""
fi
HUB_DIR="dist/hub"
DESKTOP_DIR="dist/desktop"

echo "== TPT Photo Deduper build =="
echo " edition: $([ "$PRO" = 1 ] && echo 'pro (near-duplicate UI + native bridge client)' || echo 'free (single-folder exact duplicates)')"
echo " hub bundle:  $HUB_DIR"
[ -n "$WEB_DEST" ] && echo " deploy to:   $WEB_DEST"

if [ "$PRO" = 1 ]; then
  cargo build --release --target "$TARGET" -p "$CRATE" --features pro
else
  cargo build --release --target "$TARGET" -p "$CRATE"
fi

WASM="target/$TARGET/release/tpt_app_photo_deduper.wasm"
[ -f "$WASM" ] || { echo "missing $WASM" >&2; exit 1; }

mkdir -p "$HUB_DIR"
wasm-bindgen --target web --out-name "$OUT_NAME" --out-dir "$HUB_DIR" "$WASM"

GLUE="$HUB_DIR/$OUT_NAME.js"
BG_WASM="$HUB_DIR/${OUT_NAME}_bg.wasm"
[ -f "$GLUE" ] || { echo "wasm-bindgen did not produce $GLUE" >&2; exit 1; }
[ -f "$BG_WASM" ] || { echo "wasm-bindgen did not produce $BG_WASM" >&2; exit 1; }

if [ -n "$WEB_DEST" ]; then
  mkdir -p "$WEB_DEST"
  cp "$GLUE" "$BG_WASM" "$WEB_DEST/"
  echo "-- copied hub bundle into $WEB_DEST"
fi

RAW=$(wc -c < "$BG_WASM" | tr -d ' ')
if command -v gzip >/dev/null 2>&1; then
  GZ=$(gzip -9 -c "$BG_WASM" | wc -c | tr -d ' ')
  echo "-- $(basename "$BG_WASM"): $RAW bytes raw, $GZ bytes gzipped"
  if [ "$GZ" -gt 409600 ]; then
    echo "warning: payload exceeds the ~400 KB gzip target" >&2
  fi
fi

if [ "$PRO" = 1 ]; then
  trunk build --release --features pro --dist "$DESKTOP_DIR"
  cargo build --release -p tpt-photo-deduper-desktop
  echo "   exe: target/release/tpt-photo-deduper-pro (serves $DESKTOP_DIR)"
fi

echo "-- done"
echo "   website upload: copy the two files from dist/hub/ into the web repo's"
echo "   public/apps/photo-deduper/ (or rerun with TPT_WEB_REPO set)."
