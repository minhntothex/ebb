#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
APP_DIR="${ROOT_DIR}/dist/Ebb.app"
BIN_SRC="${ROOT_DIR}/target/release/ebb"
PLIST_SRC="${ROOT_DIR}/packaging/Info.plist"

if [[ "${1:-}" != "--no-build" ]]; then
    (cd "${ROOT_DIR}" && cargo build --release --offline)
fi

if [[ ! -x "${BIN_SRC}" ]]; then
    echo "error: release binary not found at ${BIN_SRC}" >&2
    exit 1
fi

rm -rf "${APP_DIR}"
mkdir -p "${APP_DIR}/Contents/MacOS" "${APP_DIR}/Contents/Resources"

cp "${BIN_SRC}" "${APP_DIR}/Contents/MacOS/ebb"
chmod 0755 "${APP_DIR}/Contents/MacOS/ebb"
cp "${PLIST_SRC}" "${APP_DIR}/Contents/Info.plist"

plutil -lint "${APP_DIR}/Contents/Info.plist" >/dev/null

if command -v codesign >/dev/null 2>&1; then
    codesign --force --deep --sign - "${APP_DIR}" >/dev/null
    codesign --verify --deep --strict "${APP_DIR}"
fi

echo "Packaged: ${APP_DIR}"
ls -lh "${APP_DIR}/Contents/MacOS/ebb"
