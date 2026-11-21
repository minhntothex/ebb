#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DIST_DIR="${ROOT_DIR}/dist"
APP_DIR="${DIST_DIR}/Ebb.app"
BIN_SRC="${ROOT_DIR}/target/release/ebb"
PLIST_SRC="${ROOT_DIR}/packaging/Info.plist"
ICON_SRC="${ROOT_DIR}/packaging/AppIcon.icns"

DO_BUILD=1
MAKE_ZIP=0
MAKE_DMG=0
ARCH_LABEL=""

while [[ $# -gt 0 ]]; do
    case "$1" in
        --no-build)
            DO_BUILD=0
            shift
            ;;
        --bin)
            BIN_SRC="$2"
            DO_BUILD=0
            shift 2
            ;;
        --arch)
            ARCH_LABEL="$2"
            shift 2
            ;;
        --zip)
            MAKE_ZIP=1
            shift
            ;;
        --dmg)
            MAKE_DMG=1
            shift
            ;;
        *)
            echo "error: unknown option: $1" >&2
            echo "usage: $0 [--no-build] [--bin PATH] [--arch LABEL] [--zip] [--dmg]" >&2
            exit 2
            ;;
    esac
done

if [[ "${DO_BUILD}" -eq 1 ]]; then
    (cd "${ROOT_DIR}" && cargo build --release --offline)
fi

if [[ ! -x "${BIN_SRC}" ]]; then
    echo "error: release binary not found at ${BIN_SRC}" >&2
    exit 1
fi

if [[ ! -f "${ICON_SRC}" ]]; then
    echo "error: AppIcon.icns not found at ${ICON_SRC} (run scripts/generate-icon.swift)" >&2
    exit 1
fi

VERSION="$(/usr/libexec/PlistBuddy -c 'Print :CFBundleShortVersionString' "${PLIST_SRC}")"
if [[ -z "${ARCH_LABEL}" ]]; then
    if command -v lipo >/dev/null 2>&1; then
        ARCHS="$(lipo -archs "${BIN_SRC}" 2>/dev/null || uname -m)"
        if [[ "${ARCHS}" == *"arm64"* && "${ARCHS}" == *"x86_64"* ]]; then
            ARCH_LABEL="universal"
        else
            ARCH_LABEL="${ARCHS%% *}"
        fi
    else
        ARCH_LABEL="$(uname -m)"
    fi
fi

mkdir -p "${DIST_DIR}"
rm -rf "${APP_DIR}"
mkdir -p "${APP_DIR}/Contents/MacOS" "${APP_DIR}/Contents/Resources"

cp "${BIN_SRC}" "${APP_DIR}/Contents/MacOS/ebb"
chmod 0755 "${APP_DIR}/Contents/MacOS/ebb"
cp "${PLIST_SRC}" "${APP_DIR}/Contents/Info.plist"
cp "${ICON_SRC}" "${APP_DIR}/Contents/Resources/AppIcon.icns"

plutil -lint "${APP_DIR}/Contents/Info.plist" >/dev/null

if command -v codesign >/dev/null 2>&1; then
    codesign --force --deep --sign - "${APP_DIR}" >/dev/null
    codesign --verify --deep --strict "${APP_DIR}"
fi

echo "Packaged: ${APP_DIR} (version=${VERSION}, arch=${ARCH_LABEL})"
ls -lh "${APP_DIR}/Contents/MacOS/ebb" "${APP_DIR}/Contents/Resources/AppIcon.icns"

ARTIFACTS=()

if [[ "${MAKE_ZIP}" -eq 1 ]]; then
    ZIP_PATH="${DIST_DIR}/ebb-${VERSION}-macos-${ARCH_LABEL}.zip"
    rm -f "${ZIP_PATH}"
    ditto -c -k --sequesterRsrc --keepParent "${APP_DIR}" "${ZIP_PATH}"
    ARTIFACTS+=("$(basename "${ZIP_PATH}")")
    echo "Created ZIP: ${ZIP_PATH}"
fi

if [[ "${MAKE_DMG}" -eq 1 ]]; then
    DMG_PATH="${DIST_DIR}/ebb-${VERSION}-macos-${ARCH_LABEL}.dmg"
    DMG_STAGE="$(mktemp -d /tmp/ebb-dmg-stage.XXXXXX)"
    trap 'rm -rf "${DMG_STAGE}"' EXIT
    cp -R "${APP_DIR}" "${DMG_STAGE}/Ebb.app"
    ln -s /Applications "${DMG_STAGE}/Applications"
    rm -f "${DMG_PATH}"
    hdiutil create \
        -volname "Ebb" \
        -srcfolder "${DMG_STAGE}" \
        -ov \
        -format UDZO \
        "${DMG_PATH}" >/dev/null
    rm -rf "${DMG_STAGE}"
    trap - EXIT
    ARTIFACTS+=("$(basename "${DMG_PATH}")")
    echo "Created DMG: ${DMG_PATH}"
fi

if [[ "${#ARTIFACTS[@]}" -gt 0 ]]; then
    (cd "${DIST_DIR}" && shasum -a 256 "${ARTIFACTS[@]}" > SHA256SUMS.txt)
    echo "Checksums (${DIST_DIR}/SHA256SUMS.txt):"
    cat "${DIST_DIR}/SHA256SUMS.txt"
fi
