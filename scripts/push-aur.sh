#!/usr/bin/env bash
# Push the current daemon PKGBUILD to the AUR.
#
# Usage:  ./scripts/push-aur.sh [commit message]
# Default commit message is derived from PKGBUILD pkgver/pkgrel.
#
# Prerequisites:
#   - AUR SSH key configured (ssh://aur@aur.archlinux.org)
#   - makepkg available (pacman -S --needed base-devel)

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(dirname "$SCRIPT_DIR")"
AUR_DIR="$HOME/Development/aur/control-ofc-daemon"

# Files to sync from the main repo into the AUR repo
SYNC_FILES=(
    "packaging/PKGBUILD:PKGBUILD"
    "packaging/control-ofc-daemon.install:control-ofc-daemon.install"
)

# ── Preflight checks ────────────────────────────────────────────────

if [[ ! -d "$AUR_DIR/.git" ]]; then
    echo "error: AUR repo not found at $AUR_DIR" >&2
    echo "Clone it first:" >&2
    echo "  git clone ssh://aur@aur.archlinux.org/control-ofc-daemon.git $AUR_DIR" >&2
    exit 1
fi

if ! command -v makepkg &>/dev/null; then
    echo "error: makepkg not found (install base-devel)" >&2
    exit 1
fi

# ── Extract version from PKGBUILD ───────────────────────────────────

pkgver=$(grep -m1 '^pkgver=' "$REPO_ROOT/packaging/PKGBUILD" | cut -d= -f2)
pkgrel=$(grep -m1 '^pkgrel=' "$REPO_ROOT/packaging/PKGBUILD" | cut -d= -f2)
version="${pkgver}-${pkgrel}"
commit_msg="${1:-Update to ${version}}"

echo "==> Syncing PKGBUILD ${version} to AUR"

# ── Sync files ──────────────────────────────────────────────────────

for mapping in "${SYNC_FILES[@]}"; do
    src="${REPO_ROOT}/${mapping%%:*}"
    dst="${AUR_DIR}/${mapping##*:}"
    if [[ ! -f "$src" ]]; then
        echo "error: source file not found: $src" >&2
        exit 1
    fi
    cp "$src" "$dst"
    echo "  copied ${mapping%%:*} -> ${mapping##*:}"
done

# ── Regenerate .SRCINFO ─────────────────────────────────────────────

echo "==> Generating .SRCINFO"
(cd "$AUR_DIR" && makepkg --printsrcinfo > .SRCINFO)

# ── Show diff and confirm ───────────────────────────────────────────

echo ""
echo "==> Changes:"
(cd "$AUR_DIR" && git diff --stat)
echo ""
(cd "$AUR_DIR" && git diff)
echo ""

read -rp "Push to AUR as '${commit_msg}'? [y/N] " confirm
if [[ "$confirm" != [yY] ]]; then
    echo "Aborted."
    exit 0
fi

# ── Commit and push ─────────────────────────────────────────────────

(
    cd "$AUR_DIR"
    git add PKGBUILD control-ofc-daemon.install .SRCINFO
    git commit -m "$commit_msg"
    git push origin master
)

echo "==> Pushed ${version} to AUR"
