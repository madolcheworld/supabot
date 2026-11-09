#!/usr/bin/env bash
# Environment needed to build SwarmBot on this machine.
#
#   source scripts/env.sh
#
# gpui's build script runs bindgen (it needs libclang) and, on Linux, blade's
# Vulkan backend. This box has libclang.so.22.1 but no unversioned
# libclang.so symlink, so clang-sys needs an explicit search path.
set -euo pipefail

# Keep cargo's home inside the checkout so builds work in a sandbox that cannot
# write to ~/.cargo. It is seeded with a reflink copy of the user's registry
# (`cp -a --reflink=auto`), so nothing is re-downloaded and the original stays
# untouched (copy-on-write). Remove this line to use the global ~/.cargo.
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
export CARGO_HOME="${CARGO_HOME:-$ROOT/.cargo-home}"

export LIBCLANG_PATH="${LIBCLANG_PATH:-/usr/lib64}"

# Vulkan driver selection:
# Prefer hardware acceleration via NVIDIA if the GPU and driver are reachable;
# otherwise fall back to Mesa llvmpipe software rasterizer to keep headless/sandboxed
# sessions deterministic.
if [ -z "${VK_ICD_FILENAMES:-}" ]; then
    if [ -e /dev/nvidia0 ] && [ -f /usr/share/vulkan/icd.d/nvidia_icd.x86_64.json ]; then
        export VK_ICD_FILENAMES="/usr/share/vulkan/icd.d/nvidia_icd.x86_64.json"
    elif [ -f /usr/share/vulkan/icd.d/lvp_icd.x86_64.json ]; then
        export VK_ICD_FILENAMES="/usr/share/vulkan/icd.d/lvp_icd.x86_64.json"
    fi
fi

# The `xkbcommon` crate links `xkbcommon-x11` unconditionally on Linux, and
# this box has the runtime library but not the dev symlink
# (`libxkbcommon-x11.so`, pkg-config `xkbcommon-x11` missing). Point the linker
# at a shim directory holding that symlink. LIBRARY_PATH (unlike RUSTFLAGS) does
# not invalidate the cargo build cache, so only the final link is redone.
# The clean system fix is `sudo dnf install libxkbcommon-x11-devel`.
mkdir -p "$ROOT/.cargo-home/link-shim"
for lib in libxkbcommon-x11.so; do
    if [ ! -e "$ROOT/.cargo-home/link-shim/$lib" ]; then
        for candidate in /lib64/${lib%.so}.so.0 /usr/lib64/${lib%.so}.so.0 /usr/lib/${lib%.so}.so.0; do
            [ -e "$candidate" ] && ln -sf "$candidate" "$ROOT/.cargo-home/link-shim/$lib" && break
        done
    fi
done
export LIBRARY_PATH="$ROOT/.cargo-home/link-shim${LIBRARY_PATH:+:$LIBRARY_PATH}"

# Wayland (KDE Plasma here). GPUI is built with default-features off plus
# `font-kit` + `wayland`, because libxkbcommon-x11 dev files are missing.
export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}"

echo "LIBCLANG_PATH=$LIBCLANG_PATH"
echo "VK_ICD_FILENAMES=${VK_ICD_FILENAMES:-default}"
echo "XDG_RUNTIME_DIR=$XDG_RUNTIME_DIR"
echo "LIBRARY_PATH=$LIBRARY_PATH"
