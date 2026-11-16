#!/usr/bin/env bash
# Build the Linux release `rayline` for an old glibc and check it.
#
#   scripts/build-linux-cli.sh              build target/release/rayline and check it
#   scripts/build-linux-cli.sh --check BIN  only check BIN (a binary built elsewhere, or packaged)
#
# Runs on an x86_64 or aarch64 Linux host with Docker and the Rust toolchain installed (rustup). The build runs
# `cargo build --release -p rayline-cli --locked` inside the image pinned in release/encoder-source.json
# (linux_cuda.image, Rocky Linux 8: glibc 2.28), the one the Linux encoder builds in, with the host's rustup and cargo
# homes mounted: rustc runs on any glibc from 2.17, so nothing else is downloaded. The binary lands in
# target/release/rayline, as a host build would. The script refuses a binary that needs a glibc newer than
# linux_cuda.max_glibc, so `rayline` runs wherever the encoder does. RAYLINE_VERSION is passed through when set.
set -euo pipefail

repo="$(cd "$(dirname "$0")/.." && pwd)"
pin="$repo/release/encoder-source.json"

if [ "${1:-}" = --in-container ]; then
  # Inside the pinned image, as root: a C and C++ compiler (ring, zstd-sys and esaxx-rs build C/C++), then cargo.
  owner="$2" cargo_home="${CARGO_HOME:?}"
  dnf install -y -q gcc gcc-c++ make >/dev/null
  cd "$repo"
  cargo build --release -p rayline-cli --locked
  chown -R "$owner" "$repo/target" "$cargo_home"
  exit 0
fi

check=""
case "$#:${1:-}" in
  0:) ;;
  2:--check) check="$2" ;;
  *) echo "usage: scripts/build-linux-cli.sh [--check BIN]" >&2; exit 2 ;;
esac
command -v objdump >/dev/null || { echo "missing objdump (binutils)" >&2; exit 1; }

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
[[ "$image" =~ @sha256:[0-9a-f]{64}$ ]] || { echo "linux_cuda.image is not pinned by digest: $image" >&2; exit 1; }

# Refuses a binary that needs a glibc newer than max_glibc.
check_glibc() {
  local binary="$1" glibc newest
  [ -s "$binary" ] || { echo "missing $binary" >&2; exit 1; }
  glibc="$({ objdump -T "$binary" | grep -o 'GLIBC_[0-9.]*' || true; } | sed 's/GLIBC_//' | sort -t. -k1,1n -k2,2n -u | tail -1)"
  [ -n "$glibc" ] || { echo "$binary has no GLIBC_ symbols: not a glibc binary" >&2; exit 1; }
  newest="$(printf '%s\n%s\n' "$glibc" "$max_glibc" | sort -t. -k1,1n -k2,2n | tail -1)"
  [ "$newest" = "$max_glibc" ] || { echo "$binary needs glibc $glibc, newer than $max_glibc" >&2; exit 1; }
  echo "$binary: needs glibc $glibc (at most $max_glibc)"
}

if [ -n "$check" ]; then
  check_glibc "$check"
  exit 0
fi

[ "$(uname -s)" = Linux ] || { echo "the Linux CLI build runs on Linux" >&2; exit 1; }
for tool in docker rustup; do command -v "$tool" >/dev/null || { echo "missing $tool" >&2; exit 1; }; done
rustup_home="$(rustup show home)"
cargo_home="${CARGO_HOME:-$HOME/.cargo}"
version=()
if [ -n "${RAYLINE_VERSION+set}" ]; then version=(-e "RAYLINE_VERSION=$RAYLINE_VERSION"); fi
docker run --rm \
  -v "$repo:$repo" -v "$rustup_home:$rustup_home" -v "$cargo_home:$cargo_home" \
  -e RUSTUP_HOME="$rustup_home" -e CARGO_HOME="$cargo_home" -e PATH="$cargo_home/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin" \
  "${version[@]}" \
  -w "$repo" "$image" \
  bash "$repo/scripts/build-linux-cli.sh" --in-container "$(id -u):$(id -g)"

check_glibc "$repo/target/release/rayline"
