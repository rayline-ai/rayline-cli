# rayline-arc-encoder

The local ARC encoder: a loopback HTTP server on llama.cpp (`libllama`, static) that runs the public ARC encoder
GGUF (`rayline-ai/ARC-1.0`) and returns the encoder's final-layer state at requested token offsets. Rayline's local
ARC decisions (`routing.router = local`, PRs #291–#292) start this binary from beside the `rayline` executable
(`RAYLINE_ARC_ENCODER` names another binary); on its own it is a standalone server. Releases ship it for macOS arm64
(Metal), x86_64/aarch64 Linux and x86_64 Windows (NVIDIA CUDA, driver only). The protocol (`/health`, `/encode`, `/close`) is documented at the top
of `arc_encoder_server.cpp`.

- `build.sh`: fetches llama.cpp at the commit pinned in `../release/encoder-source.json`, verifies it and the
  patches by sha256, and builds.
- `windows/build.ps1`: the Windows release build (NVIDIA CUDA), below.
- `build-linux-release.sh`: the Linux release build. Runs `build.sh --backend cuda` in the CUDA toolkit image pinned
  by digest (Rocky Linux 8, so glibc 2.28) for the pinned architecture list, refuses any dynamic library beyond the
  driver and the system C/C++ runtimes or a newer glibc, and smoke-tests on the CPU through the toolkit's `libcuda` stub.
- `patches/0001-…`: an embeddings context skips the LM head and honors the batch's output flags (unpatched, it runs
  the 248k-token vocabulary head on every token and holds ~8 GB of logits at 8,192 tokens).
- `smoke_test.py`: starts a built binary with any small GGUF and checks the protocol.

## Build and smoke test

macOS arm64 with Xcode command line tools, CMake and Python 3:

```bash
encoder/build.sh                       # writes encoder/build/dist/rayline-arc-encoder
python3 encoder/smoke_test.py encoder/build/dist/rayline-arc-encoder [model.gguf]
```

On Linux:

```bash
encoder/build-linux-release.sh --output-dir dist --smoke-model model.gguf   # the release build; needs Docker, no GPU
encoder/build.sh --backend cuda --cuda-arch "89;121"   # on the host's CUDA toolkit; links cudart and cuBLAS statically
encoder/build.sh --backend cpu                         # development only
```

`build.sh` refuses another commit, unpinned patches, local changes and untracked files. The macOS binary links only
system frameworks, embeds its Metal shaders, needs macOS 13+, and never downloads anything. The server binds only
loopback and refuses any other `--host`. `--budget` (tokens per decode call) must equal the release's chunk size
(8192 for ARC 1.0) and not exceed `--ctx`. On Apple M4, `GGML_METAL_TENSOR_ENABLE=1` speeds up flash attention.
Retained sessions live in memory only and share one KV cache of `--ctx` tokens.

A new conversation's prompt usually starts like an earlier one (the same tool definitions and system prompt), so each
encode from position 0 keeps up to two prefix snapshots: at the end of its first readout block and at the start of
its last, each at least 1,024 tokens in. A later encode from 0 that starts with a snapshot's tokens decodes only the
rest and answers `x-restored-tokens` with the tokens it skipped (0
for none). Snapshots share KV cells with the sequences they came from, so they take no `--ctx` room of their own, and
each holds one recurrent state (about 10 MiB for ARC 1.0); at most four are kept, the least recently used replaced
first, and a request that needs KV room evicts sessions and snapshots alike, least recently used first. The ignored
test `prefix_snapshots_read_and_decide_as_a_full_encode` in `rayline-daemon` checks rows and decisions against full
encodes with any encoder binary and GGUF (`ARC_ENCODER_BIN`, `ARC_ENCODER_MODEL`, `ARC_RUNTIME_RELEASE`).

## Adding a platform

A new platform ships only after all seven steps pass on its target hardware.

1. **Build.** Add the backend to `build.sh` behind the same pin checks, with static libraries and an explicit GPU
   architecture list. Record build time and size (CUDA: ~24 min per architecture on 4 vCPUs, ~0.9 GB).
2. **Smoke test.** `smoke_test.py` passes with a small GGUF.
3. **Parity, on the target hardware.** Maintainers run the release's recorded-decision replay (the recordings aren't
   public). The bar: the same action on ≥ 256 of 265 decisions, flips only on near-ties (top-2 gap ≤ 0.001), every
   clear-margin decision agrees, identical token counts. The M4 sits at 256, so re-run on every llama.cpp or patch
   change.
4. **Speed and memory.** Time the first encode and a +2k append at 1.5k–262k tokens, with RSS and GPU memory. A 21.5k
   first encode must take about 2 s or less (CPU-only bf16 fails: 77 s at 8k).
5. **Process integration.** Shutdown works through `--parent-pipe` (stdin closing); document any environment flag
   the launcher must set.
6. **Release.** A job for that platform tag; the asset goes in the archive and `SHA256SUMS`; installers and
   `rayline update` install it only when listed; the binary is code-signed (notarized or Authenticode; Linux relies on the signed `SHA256SUMS`).
7. **End to end.** Real `claude -p` and Pi turns in local mode through the sibling binary, then an offline restart.

### Windows

`windows/build.ps1` (PowerShell 7, Visual Studio 2022 Build Tools, Git; no GPU) follows upstream llama.cpp's Windows
CUDA build: MSVC and Ninja, `GGML_NATIVE=OFF` with an explicit AVX2 baseline, `GGML_OPENMP=OFF`, the static C runtime
(`/MT`, so no VC++ redistributable) and `cudart_static`. The CUDA toolkit comes from NVIDIA's redist archives, each
pinned by sha256 in `windows_cuda` (the network installer fails unattended).
- **cuBLAS:** Windows has no static cuBLAS, so `cublas64_12.dll` (102.5 MB) and `cublasLt64_12.dll` (668.7 MB) ship
  beside the `.exe` as release assets of their own; `rayline update` and `install-rayline.ps1` install every `.dll`
  the release's `SHA256SUMS` lists.
- **Driver:** `nvcuda.dll` is delay-loaded. Without a driver (or with one older than 576, which CUDA 12.9 needs) the
  encoder runs on the CPU and reports a `cpu` engine, which the local process refuses with an error.
- **Paths:** an embedded manifest sets `activeCodePage` UTF-8 and `longPathAware`, so a model under a non-ASCII
  profile directory opens (a wide argv alone isn't enough: `std::filesystem::path` still uses the ANSI code page).
- **Process:** the local process puts the encoder and the core in a kill-on-close Job Object, so neither outlives it;
  closing stdin (`--parent-pipe`) is the graceful exit, since there is no SIGTERM.
- **Signing:** releases Authenticode-sign `rayline.exe`, `rayline-core.exe` and the encoder; the cuBLAS DLLs keep
  NVIDIA's signature.
- **Later:** Vulkan and Windows on Arm.

### Validated platforms

| Platform | Backend | Hardware | Parity (same action) | Shipped |
|---|---|---|---|---|
| macOS arm64 | Metal | M4 Max | 256/265 | yes |
| Linux aarch64 | CUDA | GB10 | 258/265 | yes (`90;121`) |
| Linux x86_64 | CUDA | L4 (sm_89) | 258/265 | yes (`75;80;86;89;90;120`) |
| Windows x86_64 | CUDA | L4 (sm_89) | 258/265 (bit-identical to Linux L4; 10–20% slower) | no |
| Linux | CPU | any | not run | no: 77 s for an 8k first encode |

## Performance reference

Indicative, from 2026-11-14 runs: the public ARC 1.0 GGUF (bf16), `--budget 8192 --ubatch 2048`, random tokens,
timed by a loopback client. **First** encodes N tokens into a new session; **+2k** appends 2,000 tokens to it. Means
of 2–4 calls (1 at 262k), `--ctx 24576` up to 21.5k and `--ctx 270000` above. The M4 Max had
`GGML_METAL_TENSOR_ENABLE=1` and was shared (load average 6–13).

| Context | M4 Max first / +2k | GB10 first / +2k | L4 first / +2k |
|---|---|---|---|
| 1.5k | 0.10 s / 0.14 s | 0.05 s / 0.07 s | 0.05 s / 0.08 s |
| 8k | 0.56 s / 0.17 s | 0.28 s / 0.08 s | 0.28 s / 0.09 s |
| 21.5k | 1.8 s / 0.23 s (2.5 s at `--ctx 270000`) | 0.73 s / 0.08 s | 0.84 s / 0.11 s |
| 64k | 8.7–12.5 s / 0.47–0.63 s | 2.9 s / 0.13 s | 4.0 s / 0.23 s |
| 262k | 134 s / 2.5 s | 22.0 s / 0.31 s | 50.1 s / 0.83 s |

Memory is fixed mostly by `--ctx`: at 24,576 the M4 Max uses 1.72 GB RSS (0.86 GB footprint) and GB10/L4 about
1.3 GB RSS plus 1.6 GB GPU; at 270,000, 5.2 GB RSS on the M4 Max and 4.0 GB GPU on GB10/L4. On M4 the tensor kernels
cut a 64k first encode from ~20 s to ~12 s.

## License

Rayline's code here (`arc_encoder_server.cpp`, `build.sh`, `smoke_test.py`, and the patch) is under the repository's
Apache License 2.0. The binary statically links llama.cpp and ggml (MIT, the ggml authors), cpp-httplib (MIT,
yhirose) and nlohmann/json (MIT, Niels Lohmann), all from the pinned llama.cpp checkout. `build.sh` copies their
license texts into `rayline-arc-encoder-LICENSE`, which ships beside the binary.
