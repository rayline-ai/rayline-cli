# Build the Windows rayline-arc-encoder (x86_64, NVIDIA CUDA): the Windows counterpart of encoder/build.sh.
#
#   encoder\windows\build.ps1 -OutputDir DIR [-CudaArch LIST] [-SourceDir DIR] [-Jobs N]
#
# Needs Visual Studio 2022 (not a later version: CUDA 12.9 refuses it) or its Build Tools (the VCTools workload, which brings CMake and Ninja) and Git. No GPU.
# release/encoder-source.json pins llama.cpp, the patches and, under windows_cuda, the CUDA toolkit: the NVIDIA redist
# archives (each checked against its sha256), the cuBLAS DLLs that ship beside the encoder and the
# CMAKE_CUDA_ARCHITECTURES list (-CudaArch replaces it; CI builds one architecture to stay fast). The build is static
# with the static C runtime (/MT, no VC++ redistributable) and cudart_static; Windows has no static cuBLAS, so
# cublas64_12.dll and cublasLt64_12.dll ship beside the exe. The driver, nvcuda.dll, is delay-loaded. It writes:
#   <OutputDir>\rayline-arc-encoder.exe       the server
#   <OutputDir>\cublas64_12.dll, cublasLt64_12.dll
#   <OutputDir>\rayline-arc-encoder-LICENSE  the licenses of the bundled third-party code (as build.sh writes it)
param(
  [Parameter(Mandatory = $true)][string]$OutputDir,
  [string]$CudaArch = "",
  [string]$SourceDir = "",
  [int]$Jobs = [Environment]::ProcessorCount
)
$ErrorActionPreference = "Stop"
$PSNativeCommandUseErrorActionPreference = $true
$here = $PSScriptRoot
$encoder = Split-Path $here -Parent
$repo = Split-Path $encoder -Parent
if (-not $SourceDir) { $SourceDir = Join-Path $encoder "build\llama.cpp" }
$pin = Get-Content (Join-Path $repo "release\encoder-source.json") -Raw | ConvertFrom-Json
$cuda = $pin.windows_cuda
if (-not $CudaArch) { $CudaArch = $cuda.cuda_arch }

function Get-Sha256([string]$Path) { (Get-FileHash $Path -Algorithm SHA256).Hash.ToLowerInvariant() }

# The MSVC x64 environment of Visual Studio 2022, imported from vcvars64.bat: CUDA 12.9's nvcc refuses newer MSVC
# versions, which a host may also have installed.
$vswhere = Join-Path ${env:ProgramFiles(x86)} "Microsoft Visual Studio\Installer\vswhere.exe"
$vs = & $vswhere -latest -products * -version "[17.0,18.0)" -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
if (-not $vs) { throw "missing Visual Studio 2022 with the VC++ x64 tools" }
$vcvars = Join-Path $vs "VC\Auxiliary\Build\vcvars64.bat"
foreach ($line in (cmd /c "`"$vcvars`" >nul && set")) {
  if ($line -match '^([^=]+)=(.*)$') { Set-Item -Path "env:$($Matches[1])" -Value $Matches[2] }
}

# The CUDA toolkit from NVIDIA's redist archives, each checked against its pinned sha256, unpacked into one root.
$cudaRoot = Join-Path $encoder "build\cuda-windows"
$downloads = Join-Path $encoder "build\cuda-downloads"
New-Item -ItemType Directory -Force $cudaRoot, $downloads | Out-Null
foreach ($archive in $cuda.archives.PSObject.Properties) {
  $zip = Join-Path $downloads (Split-Path $archive.Name -Leaf)
  if (-not (Test-Path $zip) -or (Get-Sha256 $zip) -ne $archive.Value) {
    Invoke-WebRequest -Uri ($cuda.redist + $archive.Name) -OutFile $zip -UseBasicParsing
    $actual = Get-Sha256 $zip
    if ($actual -ne $archive.Value) { throw "$($archive.Name) has sha256 $actual, pinned $($archive.Value)" }
  }
  $unpacked = Join-Path $downloads ([IO.Path]::GetFileNameWithoutExtension($zip))
  if (-not (Test-Path $unpacked)) {
    Expand-Archive $zip -DestinationPath $downloads -Force
    Copy-Item (Join-Path $unpacked "*") $cudaRoot -Recurse -Force
  }
}
$env:CUDA_PATH = $cudaRoot
$env:CUDAToolkit_ROOT = $cudaRoot
$env:PATH = "$cudaRoot\bin;$env:PATH"

# The pinned llama.cpp commit with the verified patches, as build.sh makes it.
foreach ($patch in $pin.patches.PSObject.Properties) {
  $actual = Get-Sha256 (Join-Path $encoder "patches\$($patch.Name)")
  if ($actual -ne $patch.Value) { throw "patch $($patch.Name) has sha256 $actual, pinned $($patch.Value)" }
}
if (-not (Test-Path (Join-Path $SourceDir ".git"))) {
  New-Item -ItemType Directory -Force $SourceDir | Out-Null
  git -C $SourceDir -c core.autocrlf=false init -q
  git -C $SourceDir config core.autocrlf false
  git -C $SourceDir remote add origin $pin.repository
  git -C $SourceDir fetch -q --depth 1 origin $pin.commit
  git -C $SourceDir -c advice.detachedHead=false checkout -q FETCH_HEAD
  foreach ($patch in $pin.patches.PSObject.Properties) {
    git -C $SourceDir apply (Join-Path $encoder "patches\$($patch.Name)")
  }
}
$head = git -C $SourceDir rev-parse HEAD
if ($head -ne $pin.commit) { throw "$SourceDir is at $head, pinned $($pin.commit)" }

# Static CRT, an explicit AVX2 baseline (GGML_NATIVE=OFF alone gives an SSE-only CPU backend), ggml's own thread pool
# (no OpenMP DLL), cudart_static, no NCCL.
$build = Join-Path $SourceDir "build-windows-cuda"
cmake -S $here -B $build -G Ninja -DCMAKE_BUILD_TYPE=Release "-DLLAMA_SRC=$SourceDir" "-DENCODER_DIR=$encoder" `
  -DCMAKE_MSVC_RUNTIME_LIBRARY=MultiThreaded `
  -DGGML_NATIVE=OFF -DGGML_AVX=ON -DGGML_AVX2=ON -DGGML_FMA=ON -DGGML_F16C=ON -DGGML_OPENMP=OFF -DGGML_STATIC=ON `
  -DGGML_CUDA=ON -DGGML_CUDA_NCCL=OFF "-DCMAKE_CUDA_ARCHITECTURES=$CudaArch"
cmake --build $build --target rayline-arc-encoder -j $Jobs

New-Item -ItemType Directory -Force $OutputDir | Out-Null
Copy-Item (Join-Path $build "rayline-arc-encoder.exe") $OutputDir -Force
foreach ($dll in $cuda.dlls) { Copy-Item (Join-Path $cudaRoot "bin\$dll") $OutputDir -Force }

# Byte for byte what build.sh writes (echo, printf and cat): every platform ships the same LICENSE asset.
$license = "rayline-arc-encoder bundles the third-party code below, built from llama.cpp $($pin.commit)`n" +
  "($($pin.repository)) with the patches in Rayline's encoder/patches/. Rayline's own code in it is`n" +
  "licensed under the Apache License 2.0 (see Rayline's LICENSE).`n"
foreach ($name in "LICENSE", "vendor/cpp-httplib/LICENSE", "licenses/LICENSE-jsonhpp") {
  $license += "`n==> llama.cpp/$name <==`n`n" + [IO.File]::ReadAllText((Join-Path $SourceDir $name))
}
$license += "`n==> NVIDIA CUDA Toolkit <==`n`n" +
  "The Linux and Windows builds also contain the NVIDIA CUDA runtime, cuBLAS and cuBLASLt (on Windows, cuBLAS and`n" +
  "cuBLASLt are the DLLs beside the encoder), redistributed under the NVIDIA CUDA Toolkit End User License`n" +
  "Agreement: https://docs.nvidia.com/cuda/eula/`n"
[IO.File]::WriteAllText((Join-Path $OutputDir "rayline-arc-encoder-LICENSE"), $license)

# The exe must import nothing beyond Windows itself, the delay-loaded driver and the shipped cuBLAS DLLs.
$allowed = @("kernel32.dll", "advapi32.dll", "ws2_32.dll", "nvcuda.dll") + $cuda.dlls
$imports = dumpbin /nologo /dependents (Join-Path $OutputDir "rayline-arc-encoder.exe") |
  Where-Object { $_ -match '^\s+(\S+\.dll)\s*$' } | ForEach-Object { $Matches[1].ToLowerInvariant() }
$extra = $imports | Where-Object { $allowed -notcontains $_ }
if ($extra) { throw "rayline-arc-encoder.exe imports $($extra -join ', ')" }
Get-ChildItem $OutputDir | Select-Object Name, Length | Format-Table -AutoSize
