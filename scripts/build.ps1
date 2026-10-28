# Build RealTimeX Link's executables (rtx-linkd.exe, rtx-link.exe, rtx-control.exe) for Windows, release profile,
# into OutDir. Usage: powershell -NoProfile -ExecutionPolicy Bypass -File scripts\build.ps1 [x86_64|arm64] [output_dir]
# Needs Visual Studio 2022 (or its Build Tools) with the C++ tools, and Rust (rustup), which the
# build runners already have. The C runtime is linked statically (.cargo/config.toml).
# Native tools write progress to stderr, which Windows PowerShell 5.1 turns into error records when
# output is redirected (CI): failures are detected from exit codes.
param(
  [string]$Arch = 'x86_64',
  [string]$OutDir = 'dist'
)

function Fail([string]$Message) { Write-Host "ERROR: $Message"; exit 1 }
function Run {
  $exe = $args[0]
  $rest = @($args | Select-Object -Skip 1)
  & $exe @rest
  if ($LASTEXITCODE -ne 0) { Fail "$exe $($rest -join ' ') failed (exit $LASTEXITCODE)" }
}

switch ($Arch) {
  { $_ -in 'x86_64', 'amd64', 'x64' } { $Arch = 'x86_64'; $Target = 'x86_64-pc-windows-msvc'; $VsArch = 'x64' }
  { $_ -in 'arm64', 'aarch64' } { $Arch = 'arm64'; $Target = 'aarch64-pc-windows-msvc'; $VsArch = 'arm64' }
  default { Fail "Invalid architecture '$Arch'. Use 'x86_64' or 'arm64'" }
}
if (-not [IO.Path]::IsPathRooted($OutDir)) { $OutDir = Join-Path (Get-Location) $OutDir }

# The GitLab runner service (LocalSystem) keeps Rust in ProgramData, where RealTimeX's own native
# build installed it (its profile under System32 is redirected for 32-bit tools).
if ($env:USERPROFILE -and $env:SystemRoot -and $env:USERPROFILE.StartsWith($env:SystemRoot, [StringComparison]::OrdinalIgnoreCase)) {
  $shared = Join-Path $env:ProgramData 'rtx-capture'
  if (-not $env:CARGO_HOME) { $env:CARGO_HOME = Join-Path $shared 'cargo' }
  if (-not $env:RUSTUP_HOME) { $env:RUSTUP_HOME = Join-Path $shared 'rustup' }
}
$cargoHome = if ($env:CARGO_HOME) { $env:CARGO_HOME } else { Join-Path $env:USERPROFILE '.cargo' }
$env:PATH = "$(Join-Path $cargoHome 'bin');$env:PATH"
if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) { Fail 'Rust (rustup) is not installed on this machine' }

# The Visual Studio C++ environment for the target.
if (-not ($env:VSCMD_ARG_TGT_ARCH -eq $VsArch -and (Get-Command cl.exe -ErrorAction SilentlyContinue))) {
  $vswhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio\Installer\vswhere.exe'
  $component = if ($Arch -eq 'arm64') { 'Microsoft.VisualStudio.Component.VC.Tools.ARM64' } else { 'Microsoft.VisualStudio.Component.VC.Tools.x86.x64' }
  $vs = if (Test-Path $vswhere) { & $vswhere -latest -products * -requires $component -property installationPath } else { $null }
  if (-not $vs) { Fail "no Visual Studio C++ tools for $Arch ($component)" }
  $hostArch = if ($env:PROCESSOR_ARCHITECTURE -eq 'ARM64') { 'arm64' } else { 'x64' }
  $vcvarsArch = if ($hostArch -eq $VsArch) { $VsArch } else { "${hostArch}_$VsArch" }
  $script = Join-Path ([IO.Path]::GetTempPath()) "link-vcvars-$PID.cmd"
  Set-Content -LiteralPath $script -Encoding ASCII -Value "@call `"$(Join-Path $vs 'VC\Auxiliary\Build\vcvarsall.bat')`" $vcvarsArch >nul || exit /b 1`r`n@set"
  $lines = & cmd.exe /d /c $script
  $code = $LASTEXITCODE
  Remove-Item -LiteralPath $script -ErrorAction SilentlyContinue
  if ($code -ne 0) { Fail "vcvarsall $vcvarsArch failed (exit $code)" }
  foreach ($line in $lines) { if ($line -match '^([^=]+)=(.*)$') { Set-Item -LiteralPath "env:$($Matches[1])" -Value $Matches[2] } }
}

Set-Location (Join-Path $PSScriptRoot '..')
Write-Host "Building rtx-linkd, rtx-link and rtx-control for $Target into $OutDir"
Run rustup target add $Target
Run cargo build --release --locked --target $Target -p rtx-linkd
Run cargo build --release --locked --target $Target -p rtx-link
Run cargo build --release --locked --target $Target -p rtx-control
# The Windows key mapping's unit tests (physical keys by scancode, #2909), when CI asks for them.
if ($env:LINK_TEST_KEYS -eq '1') {
  Run cargo test --release --locked --target $Target -p rtx-control '--' platform::windows::keymap codes::
}
$targetDir = if ($env:CARGO_TARGET_DIR) { $env:CARGO_TARGET_DIR } else { 'target' }
$built = Join-Path $targetDir "$Target\release"
New-Item -ItemType Directory -Force -Path $OutDir | Out-Null
foreach ($name in 'rtx-linkd.exe', 'rtx-link.exe', 'rtx-control.exe') {
  $exe = Join-Path $OutDir $name
  Copy-Item -LiteralPath (Join-Path $built $name) -Destination $exe -Force
  $dlls = & dumpbin.exe /nologo /dependents $exe | Where-Object { $_ -match '^\s+\S+\.dll\s*$' } | ForEach-Object { $_.Trim() }
  $crt = @($dlls | Where-Object { $_ -match '^(vcruntime|msvcp|ucrtbase|api-ms-win-crt-)' })
  if ($crt.Count -gt 0) { Fail "$name depends on the C runtime DLLs: $($crt -join ', ')" }
}
Run (Join-Path $OutDir 'rtx-linkd.exe') --version
Run (Join-Path $OutDir 'rtx-control.exe') --version
Write-Host "SUCCESS: $(Join-Path $OutDir 'rtx-linkd.exe') $(Join-Path $OutDir 'rtx-link.exe') $(Join-Path $OutDir 'rtx-control.exe')"
