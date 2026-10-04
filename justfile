# TPT Photo Deduper - task runner.
# The pipeline logic lives in build.ps1 (Windows) / build.sh (unix); these
# recipes are thin wrappers so `just` works everywhere.
#
# Outputs (never overwrite each other):
#   dist\hub\      free wasm bundle for the website
#   dist\desktop\  trunk bundle the Pro exe serves
#   release\       staged Pro exe package + zip (-Pro / just pro)

set windows-shell := ["pwsh.exe", "-NoLogo", "-NoProfile", "-Command"]

default:
    @just --list

# Build the free WASM edition into dist\hub (+ gzip size report).
build:
    pwsh -NoLogo -NoProfile -File build.ps1

# Build the Pro edition: near-duplicate UI, trunk bundle in dist\desktop,
# exe, and a zip-ready package in release\.
pro:
    pwsh -NoLogo -NoProfile -File build.ps1 -Pro

# Copy the hub bundle into the web repo (set TPT_WEB_REPO first).
deploy:
    @if (-not $env:TPT_WEB_REPO) { Write-Error "set `$env:TPT_WEB_REPO to the web repo checkout first"; exit 1 }
    pwsh -NoLogo -NoProfile -File build.ps1 -SkipBuild -WebRepo:$env:TPT_WEB_REPO

# Re-run only wasm-bindgen + copy (after an existing release build).
bindgen:
    pwsh -NoLogo -NoProfile -File build.ps1 -SkipBuild

# Host test suites: engine (baseline) + engine (all features) + app unit tests.
test:
    cargo test -p tpt-photo-dedupe
    cargo test -p tpt-photo-dedupe --all-features
    cargo test -p tpt-app-photo-deduper

# Type-check the wasm lib (free), the pro wasm lib, and the desktop shell.
check:
    cargo check --target wasm32-unknown-unknown -p tpt-app-photo-deduper
    cargo check --target wasm32-unknown-unknown --features pro -p tpt-app-photo-deduper
    cargo check -p tpt-photo-deduper-desktop

# Live-reload standalone dev server (index.html mounts via #tpt-appfront-root).
dev:
    trunk serve

# Live-reload dev server with the Pro UI (no native bridge - browser scan
# path with canvas perceptual hashing; delete stays desktop-only).
dev-pro:
    trunk serve --features pro

# Remove build outputs.
clean:
    cargo clean
    -rd /s /q dist 2>nul || rm -rf dist
    -rd /s /q release 2>nul || rm -rf release
