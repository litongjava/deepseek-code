#!/usr/bin/env bash
# Fetches PRoot and its libraries for the Android app's Linux environment.
#
# Android only executes files from the APK's native library directory, so PRoot
# is packaged as lib*.so under jniLibs. The binaries come from Termux packages
# (built against Android's libc), pinned by SHA-256, and are patched so their
# library names and search paths fit the APK layout.
#
# Usage: scripts/fetch-android-proot.sh [work-dir]
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
APP="$ROOT/src-tauri/gen/android/app/src/main"
WORK="${1:-$ROOT/target/android-proot}"
POOL="https://packages.termux.dev/apt/termux-main/pool/main"
PATCHELF_URL="https://github.com/NixOS/patchelf/releases/download/0.19.3/patchelf-0.19.3-x86_64.tar.gz"
PATCHELF_SHA="38861b171079e440b25ac86483284f690d0ee7921dba98b0c8fe04a9b0a11667"

# package-path  file  sha256
PACKAGES=(
  "p/proot proot_5.1.107.102_x86_64.deb 2497064226c15971be8131c7facead94a06c225347796326a383c832dca57758"
  "p/proot proot_5.1.107.102_aarch64.deb 10a394b585c2fc2b2c8502544f4e5f1be581dc8bfff79e8ba62f16207423e030"
  "libt/libtalloc libtalloc_2.5.0_x86_64.deb b8c6d95f20075dc1f9ec6573575b2444e8d526e48e0d8d6d5cf4e071e6e06530"
  "libt/libtalloc libtalloc_2.5.0_aarch64.deb 556591f43bb773ad8777e1a29522640866a55f95dab71914418b94a8c58ad5a7"
  "liba/libandroid-shmem libandroid-shmem_0.7_x86_64.deb ffa9e4c87467b158b148d0ff92dda796aa038276c2075af3269cdcdb06f25797"
  "liba/libandroid-shmem libandroid-shmem_0.7_aarch64.deb 0da3a24d558b93c92bcf8d611e0826a99ff96e396b148e6cdf33b47c47c57ff6"
)

fetch() { # url file sha256
  if [ ! -f "$2" ] || ! echo "$3  $2" | sha256sum -c --status; then
    curl -sSfL --retry 3 -o "$2" "$1" || { echo "Download failed: $1" >&2; exit 1; }
  fi
  echo "$3  $2" | sha256sum -c --status || { echo "Checksum mismatch: $2" >&2; exit 1; }
}

mkdir -p "$WORK"
cd "$WORK"
fetch "$PATCHELF_URL" patchelf.tar.gz "$PATCHELF_SHA"
mkdir -p patchelf && tar xzf patchelf.tar.gz -C patchelf
PATCHELF="$WORK/patchelf/bin/patchelf"

for entry in "${PACKAGES[@]}"; do
  read -r dir file sha <<<"$entry"
  fetch "$POOL/$dir/$file" "$file" "$sha"
  rm -rf "x/${file%.deb}" && mkdir -p "x/${file%.deb}"
  (cd "x/${file%.deb}" && ar x "$WORK/$file" && tar xf data.tar.*)
done

usr=data/data/com.termux/files/usr
for pair in x86_64:x86_64 aarch64:arm64-v8a; do
  arch=${pair%%:*}; abi=${pair##*:}; out="$APP/jniLibs/$abi"
  mkdir -p "$out"
  cp "x/proot_5.1.107.102_$arch/$usr/bin/proot" "$out/libproot.so"
  cp "x/proot_5.1.107.102_$arch/$usr/libexec/proot/loader" "$out/libproot-loader.so"
  cp "x/libtalloc_2.5.0_$arch/$usr/lib/libtalloc.so.2.5.0" "$out/libtalloc.so"
  cp "x/libandroid-shmem_0.7_$arch/$usr/lib/libandroid-shmem.so" "$out/libandroid-shmem.so"
  "$PATCHELF" --replace-needed libtalloc.so.2 libtalloc.so --remove-rpath "$out/libproot.so"
  "$PATCHELF" --set-soname libtalloc.so --remove-rpath "$out/libtalloc.so"
  "$PATCHELF" --remove-rpath "$out/libandroid-shmem.so"
done

# Ship the licenses of the bundled components with the app.
licenses="$APP/assets/licenses"
mkdir -p "$licenses"
cp "x/libandroid-shmem_0.7_x86_64/$usr/share/doc/libandroid-shmem/copyright" "$licenses/libandroid-shmem.txt"
# Termux points the other copyright files at a shared licenses package; fetch
# the texts it names (proot: GPL-2.0, talloc: GPL-3.0) directly.
fetch "https://raw.githubusercontent.com/termux/proot/master/COPYING" proot-COPYING \
  0ffa6454acbe7e410ac6f0007df83b9b3161fe55484c1f4b87487c0f2d228a6d
cp proot-COPYING "$licenses/proot.txt"
fetch "https://www.gnu.org/licenses/gpl-3.0.txt" gpl-3.0.txt \
  3972dc9744f6499f0f9b2dbf76696f2ae7ad8af9b23dde66d6af86c9dfb36986
cp gpl-3.0.txt "$licenses/talloc.txt"
cat > "$licenses/README.txt" <<'EOF'
The Linux environment bundles these components, unmodified except for their
library names and search paths:

- PRoot (GPL-2.0): https://github.com/termux/proot
- talloc (GPL-3.0, as packaged by Termux): https://talloc.samba.org
- libandroid-shmem (BSD-3-Clause): https://github.com/termux/libandroid-shmem

Binaries come from Termux packages: https://packages.termux.dev
Corresponding source: https://github.com/termux/termux-packages
EOF
echo "PRoot libraries written to $APP/jniLibs"
