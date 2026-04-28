#!/usr/bin/env bash
# bundle.sh — builds a release binary and wraps it in a macOS .app bundle.
#
# Usage:
#   ./scripts/bundle.sh [--sign "Developer ID Application: Name (TEAMID)"]
#
# Output: ./SensibleSideButtons.app
#
# Requirements: Rust toolchain, Xcode Command Line Tools.
# Optional:     A valid Developer ID certificate for codesigning + notarization.

set -euo pipefail

VERSION=$(grep '^version' Cargo.toml | head -1 | sed 's/.*= "\(.*\)"/\1/')
APP_NAME="SensibleSideButtons"
BUNDLE_ID="com.github.weiyou.sensible-side-buttons"
BINARY="sensible-side-buttons"
APP_DIR="${APP_NAME}.app"
CONTENTS="${APP_DIR}/Contents"
SIGN_ID=""

# Parse optional --sign argument.
while [[ $# -gt 0 ]]; do
    case "$1" in
        --sign) SIGN_ID="$2"; shift 2 ;;
        *) echo "Unknown argument: $1"; exit 1 ;;
    esac
done

echo "==> Building release binary (version ${VERSION})…"
cargo build --release

echo "==> Assembling ${APP_DIR}…"
rm -rf "${APP_DIR}"
mkdir -p "${CONTENTS}/MacOS" "${CONTENTS}/Resources"

cp "target/release/${BINARY}" "${CONTENTS}/MacOS/${BINARY}"

# Generate Info.plist from template.
sed "s/{{VERSION}}/${VERSION}/g" assets/Info.plist.template \
    > "${CONTENTS}/Info.plist"

# Copy icon if it exists; skip silently otherwise.
if [[ -f "assets/icon.icns" ]]; then
    cp assets/icon.icns "${CONTENTS}/Resources/icon.icns"
else
    echo "    (no assets/icon.icns found — bundle will have no icon)"
fi

echo "==> Bundle created: ${APP_DIR}"

# Optional codesigning.
if [[ -n "${SIGN_ID}" ]]; then
    echo "==> Codesigning with: ${SIGN_ID}"
    # --options runtime enables hardened runtime (required for notarization).
    codesign --deep --force --options runtime \
        --entitlements assets/entitlements.plist \
        --sign "${SIGN_ID}" \
        "${APP_DIR}"
    echo "==> Verifying signature…"
    codesign --verify --deep --strict "${APP_DIR}" && echo "    OK"
else
    echo "    Skipping codesign (pass --sign \"Developer ID Application: …\" to sign)."
fi

echo ""
echo "Done. Copy ${APP_DIR} to /Applications and grant Accessibility access."
echo "To notarize: xcrun notarytool submit ${APP_NAME}.zip --apple-id … --team-id …"
