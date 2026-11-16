#!/usr/bin/env bash
# Build the Linux release rayline-arc-encoder (NVIDIA CUDA) and check it.
#
#   encoder/build-linux-release.sh --output-dir DIR [--smoke-model F.gguf] [--cuda-arch LIST] [--check-only]
#
# Runs on an x86_64 or aarch64 Linux host with Docker; no GPU is needed. The build runs build.sh --backend cuda inside
# the CUDA toolkit image pinned in release/encoder-source.json (linux_cuda.image, Rocky Linux 8: glibc 2.28), for this
# architecture's pinned CMAKE_CUDA_ARCHITECTURES list. The CUDA runtime and cuBLAS are linked statically, so the binary
# needs only the NVIDIA driver (libcuda.so.1) and the system C and C++ runtimes. cuBLAS is first pruned (nvprune) to the
# GPU generations the arch list needs. The script refuses a binary that
# links anything else or needs a glibc newer than linux_cuda.max_glibc. With --smoke-model it then runs smoke_test.py
# against the toolkit's libcuda stub: the stub reports no GPU, so ggml falls back to the CPU and the protocol is
# checked without a GPU. --cuda-arch replaces the pinned list (CI builds one architecture to stay fast).
# --check-only skips the build and checks an encoder already in DIR (a cached build): DIR holds rayline-arc-encoder,
# rayline-arc-encoder-LICENSE and stub/libcuda.so.1, as a build leaves them.
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
repo="$(cd "$here/.." && pwd)"
pin="$repo/release/encoder-source.json"
out=""
smoke_model=""
arch=""
check_only=""

if [ "${1:-}" = --in-container ]; then
  # Inside the pinned image, as root: the build tools, then build.sh. Outputs are handed back to the caller's user.
  arch="$2" owner="$3"
  dnf install -y -q git cmake python3 make findutils binutils gcc-toolset-12-gcc-c++ >/dev/null
  # shellcheck disable=SC1091
  source /opt/rh/gcc-toolset-12/enable
  # A rerun finds the llama.cpp checkout owned by the caller (chowned below).
  git config --global --add safe.directory "*"
  # Static cuBLAS/cuBLASLt carry kernels for every GPU generation (most of the binary's size). Keep, for each target,
  # every architecture the library has with the target's major version and no newer minor, suffixed variants (sm_90a)
  # included: cuBLAS may pick an older minor's kernel on a newer GPU of the same major. This prunes the container's
  # copies before the link.
  for lib in libcublas_static.a libcublasLt_static.a; do
    file="/usr/local/cuda/lib64/$lib"
    present="$(cuobjdump --list-elf "$file" | grep -o -E 'sm_[0-9]+[a-z]?' | sed 's/sm_//' | sort -u | sort -n | paste -sd' ' -)"
    keep=()
    for target in ${arch//;/ }; do
      found=""
      for a in $present; do
        n="${a%%[a-z]*}"
        if [ $((n / 10)) -eq $((target / 10)) ] && [ "$n" -le "$target" ]; then keep+=("$a"); found=1; fi
      done
      [ -n "$found" ] || { echo "$lib has no kernels usable on sm_$target (has: $present)" >&2; exit 1; }
    done
    gencode=()
    for a in $(printf '%s\n' "${keep[@]}" | sort -u); do gencode+=(-gencode "arch=compute_$a,code=sm_$a"); done
    before="$(du -m "$file" | cut -f1)"
    cp "$file" "$file.orig"
    nvprune "${gencode[@]}" -o "$file.pruned" "$file"
    mv "$file.pruned" "$file"
    echo "$lib: kept sm $(printf '%s\n' "${keep[@]}" | sort -u | sort -n | paste -sd' ' -) of sm $present; ${before} MB -> $(du -m "$file" | cut -f1) MB"
  done
  "$here/build.sh" --backend cuda --cuda-arch "$arch" --output-dir /out
  # For the record: the size without pruning (a relink against the original libraries; nothing recompiles).
  for lib in libcublas_static.a libcublasLt_static.a; do
    mv "/usr/local/cuda/lib64/$lib.orig" "/usr/local/cuda/lib64/$lib"
  done
  "$here/build.sh" --backend cuda --cuda-arch "$arch" --output-dir /tmp/unpruned >/dev/null
  echo "rayline-arc-encoder: $(du -m /out/rayline-arc-encoder | cut -f1) MB with cuBLAS pruned," \
    "$(du -m /tmp/unpruned/rayline-arc-encoder | cut -f1) MB without"
  mkdir -p /out/stub
  cp /usr/local/cuda/lib64/stubs/libcuda.so /out/stub/libcuda.so.1
  chown -R "$owner" /out "$here/build"
  exit 0
fi

while [ $# -gt 0 ]; do
  case "$1" in
    --output-dir) out="$2"; shift 2 ;;
    --smoke-model) smoke_model="$2"; shift 2 ;;
    --cuda-arch) arch="$2"; shift 2 ;;
    --check-only) check_only=1; shift ;;
    -h|--help) sed -n '2,16p' "$0"; exit 0 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done
[ -n "$out" ] || { echo "--output-dir is required" >&2; exit 2; }
[ "$(uname -s)" = Linux ] || { echo "the Linux release build runs on Linux" >&2; exit 1; }
for tool in readelf objdump; do command -v "$tool" >/dev/null || { echo "missing $tool (binutils)" >&2; exit 1; }; done
[ -n "$check_only" ] || command -v docker >/dev/null || { echo "missing docker" >&2; exit 1; }

machine="$(uname -m)"
case "$machine" in x86_64|aarch64) ;; *) echo "unsupported architecture: $machine" >&2; exit 1 ;; esac
pin_value() {
  python3 -I -c '
import json, sys
value = json.load(open(sys.argv[1]))
for key in sys.argv[2:]:
    value = value[key]
print(value)' "$pin" "$@"
}
image="$(pin_value linux_cuda image)"
max_glibc="$(pin_value linux_cuda max_glibc)"
[ -n "$arch" ] || arch="$(pin_value linux_cuda cuda_arch "$machine")"
[[ "$image" =~ @sha256:[0-9a-f]{64}$ ]] || { echo "linux_cuda.image is not pinned by digest: $image" >&2; exit 1; }

mkdir -p "$out"
out="$(cd "$out" && pwd)"
if [ -z "$check_only" ]; then
  docker run --rm -v "$repo:$repo" -v "$out:/out" -w "$repo" "$image" \
    bash "$here/build-linux-release.sh" --in-container "$arch" "$(id -u):$(id -g)"
fi

binary="$out/rayline-arc-encoder"
for f in "$binary" "$out/rayline-arc-encoder-LICENSE" "$out/stub/libcuda.so.1"; do
  [ -s "$f" ] || { echo "missing $f" >&2; exit 1; }
done

# Only the driver and the system C/C++ runtimes may be linked dynamically.
allowed='^(libcuda\.so\.1|libc\.so\.6|libm\.so\.6|libdl\.so\.2|librt\.so\.1|libpthread\.so\.0|libgcc_s\.so\.1|libstdc\+\+\.so\.6|ld-linux-(x86-64|aarch64)\.so\.[12])$'
needed="$(readelf -d "$binary" | awk '/\(NEEDED\)/ { gsub(/[][]/, "", $5); print $5 }')"
if grep -vE "$allowed" <<< "$needed"; then
  echo "rayline-arc-encoder links a library outside the driver and the system runtimes (above)" >&2
  exit 1
fi
glibc="$(objdump -T "$binary" | grep -o 'GLIBC_[0-9.]*' | sed 's/GLIBC_//' | sort -t. -k1,1n -k2,2n -u | tail -1)"
newest="$(printf '%s\n%s\n' "$glibc" "$max_glibc" | sort -t. -k1,1n -k2,2n | tail -1)"
[ "$newest" = "$max_glibc" ] || { echo "rayline-arc-encoder needs glibc $glibc, newer than $max_glibc" >&2; exit 1; }
echo "rayline-arc-encoder: CUDA archs $arch, needs glibc $glibc, links: $(tr '\n' ' ' <<< "$needed")"

if [ -n "$smoke_model" ]; then
  LD_LIBRARY_PATH="$out/stub" python3 -I "$here/smoke_test.py" "$binary" "$smoke_model"
fi
# stub/ stays: a cache of this build keeps it for --check-only.
echo "$binary"
