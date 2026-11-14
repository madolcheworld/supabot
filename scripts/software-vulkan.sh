#!/usr/bin/env bash
# Stage Mesa's software Vulkan driver (lavapipe) inside the checkout, so
# `swarmbot-ui --selftest` can open a window on a machine with no reachable GPU
# — a sandbox whose `/dev` has no DRM node, a headless box, CI — without root and
# without touching the system.
#
#   scripts/software-vulkan.sh              # stage it (downloads if needed)
#   scripts/software-vulkan.sh --offline    # only use what is already here
#   scripts/software-vulkan.sh --status     # what would happen, change nothing
#   scripts/software-vulkan.sh --clean      # remove the staged tree
#
# Why this is a script and not a README paragraph: the failure it prevents is a
# panic ("Unable to init GPU context: NoSupportedDeviceFound") that says nothing
# about the machine, and the fix has three steps that are easy to get wrong
# (extract the package, *generate* an ICD file with an absolute library path,
# export the variable). `scripts/env.sh` picks the staged ICD up automatically,
# so the whole recipe an operator needs is:
#
#   scripts/software-vulkan.sh && source scripts/env.sh
#
# What it stages, and why the ICD file is generated rather than copied:
#
#   .tmp-ui-gpu/usr/lib/libvulkan_lvp.so        the driver (from the package)
#   .tmp-ui-gpu/lvp_local.json                  an ICD naming it by ABSOLUTE path
#
# Arch's `lvp_icd.json` names its library as a bare soname (`libvulkan_lvp.so`),
# which the loader resolves through the dynamic linker's search path. Relying on
# that would mean putting a package's `usr/lib` on `LD_LIBRARY_PATH` for every
# process in the shell — which can shadow real system libraries. An ICD file with
# an absolute path needs neither, so this script writes one.
#
# `.tmp-*` is gitignored (see .gitignore), so nothing staged here is ever
# committed. `pacman -Sp` resolves the download URL without root; it is only read
# when `--offline` was not asked for, and the URL is printed before it is used.
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
STAGE="$ROOT/.tmp-ui-gpu"
LIB="$STAGE/usr/lib/libvulkan_lvp.so"
ICD="$STAGE/lvp_local.json"
PKG_NAME="vulkan-swrast"

say() { printf '%s\n' "$*"; }
fail() {
    printf 'software-vulkan: %s\n' "$*" >&2
    exit 1
}

mode="stage"
for arg in "$@"; do
    case "$arg" in
        --offline) mode="offline" ;;
        --status) mode="status" ;;
        --clean) mode="clean" ;;
        -h | --help)
            sed -n '2,40p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
            exit 0
            ;;
        *)
            fail "unknown argument \`$arg\` (try --help)"
            ;;
    esac
done

if [ "$mode" = "clean" ]; then
    rm -rf "$STAGE"
    say "removed $STAGE"
    exit 0
fi

# The generated ICD must name an absolute path that still exists: the checkout
# may have been moved since it was written, in which case it is regenerated.
write_icd() {
    mkdir -p "$STAGE"
    cat >"$ICD" <<EOF
{
    "file_format_version": "1.0.1",
    "ICD": {
        "api_version": "1.4.0",
        "library_path": "$LIB"
    }
}
EOF
}

icd_is_current() {
    [ -f "$ICD" ] && grep -q "\"library_path\": \"$LIB\"" "$ICD"
}

status() {
    say "stage dir:  $STAGE"
    [ -f "$LIB" ] && say "driver:     present ($LIB)" || say "driver:     absent"
    [ -f "$ICD" ] && say "icd file:   present ($ICD)" || say "icd file:   absent"
    if icd_is_current; then
        say "state:      ready — \`source scripts/env.sh\` will use it"
    else
        say "state:      not staged"
    fi
}

if [ "$mode" = "status" ]; then
    status
    exit 0
fi

if [ -f "$LIB" ] && icd_is_current; then
    say "software-vulkan: already staged — \`source scripts/env.sh\` picks it up"
    exit 0
fi

# 1. Extract a package already sitting in the stage dir (a previous download, or
#    one an operator copied in). `bsdtar` reads the .pkg.tar.zst container the
#    way `pacman -U` would, without installing anything.
if [ ! -f "$LIB" ]; then
    pkg="$(find "$STAGE" -maxdepth 1 -name "${PKG_NAME}-*.pkg.tar.zst" -print -quit 2>/dev/null)"
    if [ -z "$pkg" ] && [ "$mode" != "offline" ]; then
        url="$(pacman -Sp "$PKG_NAME" 2>/dev/null | tail -1)"
        if [ -n "$url" ]; then
            say "software-vulkan: fetching $PKG_NAME"
            say "  from    $url"
            mkdir -p "$STAGE"
            if ! curl -fsSL -o "$STAGE/$(basename "$url")" "$url"; then
                fail "download failed: $url (re-run with --offline to use a package you already have)"
            fi
            pkg="$STAGE/$(basename "$url")"
        fi
    fi
    if [ -z "$pkg" ]; then
        fail "cannot find $PKG_NAME: no local package and no download URL. Install Mesa's software Vulkan driver (Arch: \`sudo pacman -S vulkan-swrast\`) or put the .pkg.tar.zst in $STAGE and re-run."
    fi
    say "software-vulkan: extracting $(basename "$pkg")"
    # Arch packages are zstd-compressed tars; bsdtar and GNU tar --zstd both read
    # them. Extracting into the stage dir yields a relocated ./usr tree.
    if ! (cd "$STAGE" && bsdtar -xf "$pkg" 2>/dev/null || tar --zstd -xf "$pkg"); then
        fail "cannot extract $pkg (needs bsdtar or tar with zstd support)"
    fi
    [ -f "$LIB" ] || fail "extracted $pkg but $LIB is missing — unexpected package layout"
fi

write_icd
say "software-vulkan: ready"
say "  driver  $LIB"
say "  icd     $ICD"
say "next: source scripts/env.sh   # exports VK_ICD_FILENAMES for you"
