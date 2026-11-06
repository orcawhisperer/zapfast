#!/bin/sh
# Builds the browser version into dist/web. Pass `demo` for the sample chats
# (open the page with ?demo), `release` for an optimized build.
set -eu
cd "$(dirname "$0")/.."
features=web-bin
profile=dev
for arg in "$@"; do
    case "$arg" in
        demo) features=web-bin,demo ;;
        release) profile=release ;;
        *) echo "usage: web/build.sh [demo] [release]" >&2; exit 2 ;;
    esac
done
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-target/wasm}"
# A GIPHY key from .env, if there is one, goes into the program that is built, where
# anyone who loads the page can read it. Keep a personal key to a build for yourself.
if [ -z "${ZAPFAST_GIPHY_KEY:-}" ] && [ -f .env ]; then
    ZAPFAST_GIPHY_KEY=$(sed -n 's/^GIPHY_API_KEY=//p' .env | tr -d '"'"'"' \r')
    export ZAPFAST_GIPHY_KEY
fi
# wasm-bindgen reads the whole module into memory several times over, and debug
# information is most of a development build. Function names stay for stack traces.
export CARGO_PROFILE_DEV_STRIP=debuginfo
# A call's echo canceller and codec run on the page's one thread in real time. Unoptimized they take
# most of it, so a development build still optimizes these.
optimized=
for crate in sonora sonora-aec3 sonora-agc2 sonora-common-audio sonora-fft sonora-ns sonora-simd wacore; do
    optimized="$optimized --config profile.dev.package.$crate.opt-level=3"
done
# shellcheck disable=SC2086
cargo build $optimized --target wasm32-unknown-unknown --profile "$profile" \
    --bin zapfast-web --features "$features"
folder=$profile
[ "$profile" = dev ] && folder=debug
rm -rf dist/web
mkdir -p dist/web
wasm-bindgen --target web --no-typescript --out-dir dist/web \
    "$CARGO_TARGET_DIR/wasm32-unknown-unknown/$folder/zapfast-web.wasm"
cp web/static/* dist/web/
cp packaging/macos/icon-1024.png dist/web/icon-1024.png
cp packaging/icons/zapfast.svg dist/web/icon.svg
du -sh dist/web/*
