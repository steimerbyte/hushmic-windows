#!/usr/bin/env bash
# Build the Windows release into a ready-to-ship directory:
#
#   dist/hushmic-windows-x86_64/hushmic.exe
#   dist/hushmic-windows-x86_64/onnxruntime.dll
#   dist/hushmic-windows-x86_64/models/<model>.onnx
#
# That is exactly the layout `crates/hushmic/src/win/chain.rs` resolves at run
# time (`<exe_dir>\models\<stem>.onnx`, `<exe_dir>\onnxruntime.dll`), so the
# directory can be zipped and unpacked anywhere.
#
# Linux cross-compiles with cargo-xwin (MSVC CRT + SDK are downloaded to
# ~/.cache/cargo-xwin, no Windows host needed); on Windows itself it is a plain
# `cargo build`. Assets: `scripts/setup-assets.sh` provisions the models; the
# DLL has no Linux counterpart there, so point HUSHMIC_ORT_DLL at it (see
# --help) — without it the chain refuses to start and names the file.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

TARGET="x86_64-pc-windows-msvc"
DIST="$REPO_ROOT/dist/hushmic-windows-x86_64"
# The model stems the Windows build ships: the quality model (the default) and
# the light one the ladder degrades to. Same files as on Linux.
MODELS=(dpdfnet8_48khz_hr dpdfnet2_48khz_hr)
# Single source of truth for the ORT version: the Linux asset pin, so the DLL
# and the .so cannot drift apart.
ORT_VERSION="$(sed -n 's/^ORT_VERSION="\(.*\)"/\1/p' scripts/setup-assets.sh)"
ORT_ZIP="onnxruntime-win-x64-${ORT_VERSION}.zip"
ORT_URL="https://github.com/microsoft/onnxruntime/releases/download/v${ORT_VERSION}/${ORT_ZIP}"

NO_BUILD=0
EXE=""
usage() {
  cat <<EOF
hushmic Windows build

Usage:
  $0                 Build and stage dist/hushmic-windows-x86_64/
  $0 --no-build      Stage from an existing target/ build
  $0 -h | --help     This text

Environment:
  HUSHMIC_ORT_DLL    Path to onnxruntime.dll (default: assets/win/onnxruntime.dll
                     or the unpacked ${ORT_ZIP})
EOF
}

while [ $# -gt 0 ]; do
  case "$1" in
    --no-build) NO_BUILD=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) echo "unknown argument: $1" >&2; usage >&2; exit 2 ;;
  esac
done

# Fail before the (long) compile when an asset is missing: the source of a
# Windows binary that cannot start is the DLL/model, never the code.
for m in "${MODELS[@]}"; do
  if [ ! -f "$REPO_ROOT/assets/models/$m.onnx" ]; then
    echo "ERROR: missing assets/models/$m.onnx" >&2
    echo "       run scripts/setup-assets.sh first (it fetches and hash-verifies" >&2
    echo "       every model the Linux and Windows builds ship)." >&2
    exit 1
  fi
done

# The DLL has no Linux counterpart in setup-assets.sh: the same file must be
# obtained from Microsoft's release archive. Overrides win, then the repo-local
# spot an unpacked archive lands in.
ORT_DLL=""
for c in "${HUSHMIC_ORT_DLL:-}" \
         "$REPO_ROOT/assets/win/onnxruntime.dll" \
         "$REPO_ROOT/assets/win/onnxruntime-win-x64-${ORT_VERSION}/lib/onnxruntime.dll"; do
  if [ -n "$c" ] && [ -f "$c" ]; then
    ORT_DLL="$c"
    break
  fi
done
if [ -z "$ORT_DLL" ]; then
  echo "ERROR: onnxruntime.dll not found — the engine loads it from next to hushmic.exe" >&2
  echo "       (ORT_DYLIB_PATH overrides that path at run time)." >&2
  echo "       Expected at: assets/win/onnxruntime.dll, or set HUSHMIC_ORT_DLL." >&2
  echo "       Obtain it from: $ORT_URL" >&2
  echo "         curl -fsSLo /tmp/$ORT_ZIP $ORT_URL && \\" >&2
  echo "           unzip -jo /tmp/$ORT_ZIP 'onnxruntime-win-x64-${ORT_VERSION}/lib/onnxruntime.dll' -d assets/win" >&2
  exit 1
fi

if [ "$NO_BUILD" = 0 ]; then
  case "$(uname -s)" in
    Linux*|Darwin*)
      # cargo-xwin links with lld-link against the downloaded MSVC SDK libs.
      EXE="$REPO_ROOT/target/$TARGET/release/hushmic.exe"
      echo "Building $TARGET with cargo xwin (cross-compile)..."
      cargo xwin build --release --target "$TARGET" -p hushmic
      ;;
    MINGW*|MSYS*|CYGWIN*)
      EXE="$REPO_ROOT/target/release/hushmic.exe"
      echo "Building the native Windows target..."
      cargo build --release -p hushmic
      ;;
    *)
      echo "ERROR: unsupported host '$(uname -s)': build on Linux (cargo xwin) or Windows" >&2
      exit 1
      ;;
  esac
else
  # --no-build: whichever target dir already holds an EXE.
  for c in "$REPO_ROOT/target/$TARGET/release/hushmic.exe" \
           "$REPO_ROOT/target/release/hushmic.exe"; do
    if [ -f "$c" ]; then
      EXE="$c"
      break
    fi
  done
fi

if [ -z "$EXE" ] || [ ! -f "$EXE" ]; then
  echo "ERROR: hushmic.exe not built — expected target/$TARGET/release/hushmic.exe" >&2
  echo "       (or target/release/hushmic.exe on Windows). Build it first." >&2
  exit 1
fi

rm -rf "$DIST"
mkdir -p "$DIST/models"
cp "$EXE" "$DIST/hushmic.exe"
cp "$ORT_DLL" "$DIST/onnxruntime.dll"
for m in "${MODELS[@]}"; do
  cp "$REPO_ROOT/assets/models/$m.onnx" "$DIST/models/$m.onnx"
done

missing=0
FILES=(hushmic.exe onnxruntime.dll)
for m in "${MODELS[@]}"; do
  FILES+=("models/$m.onnx")
done
for rel in "${FILES[@]}"; do
  if [ -f "$DIST/$rel" ]; then
    printf '  %-56s %s\n' "$rel" "$(du -h "$DIST/$rel" | cut -f1)"
  else
    echo "MISSING: $DIST/$rel" >&2
    missing=1
  fi
done
if [ "$missing" != 0 ]; then
  echo "ERROR: the staged directory is incomplete" >&2
  exit 1
fi

echo "Ready: ${DIST#"$REPO_ROOT"/}"
echo "Unpack it anywhere and run hushmic.exe --doctor first."
