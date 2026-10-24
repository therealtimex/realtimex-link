# Sign RealTimeX Link for Windows, on RealTimeX's Windows release runner (the GitLab `sign-win`
# job): the unsigned archive in; out, the archive (same name) with the executables signed by Azure
# Trusted Signing, and RealTimeX-Link-Setup-<version>.exe, Link's NSIS installer around them,
# signed. Signing is the one RealTimeX's electron-builder uses: the TrustedSigning module (pinned
# below), RealTimeX's account and profile, AZURE_TENANT_ID / AZURE_CLIENT_ID / AZURE_CLIENT_SECRET.
# -In is the verify job's output (ci/release.py fetch): the unsigned archive, taken from the tag's
# build run and checked there, and tree\, the release commit's packaging\ and its icon (the
# installer script comes from there, not from this checkout, which is main's when signing again).
param(
  [Parameter(Mandatory = $true)][string]$Version,
  [Parameter(Mandatory = $true)][string]$In,
  [Parameter(Mandatory = $true)][string]$Out
)
$ErrorActionPreference = 'Stop'
$name = 'realtimex-link-windows-x86_64.tar.gz'
$setup = "RealTimeX-Link-Setup-$Version.exe"
$publisher = 'Dang Le'
$executables = @('rtx-linkd.exe', 'rtx-link.exe', 'rtx-control.exe')
# The TrustedSigning module, exactly this version (PowerShell Gallery): a new upload under that name
# never runs with the signing credentials until it is reviewed and pinned here.
$trustedSigningVersion = '0.5.8'

foreach ($variable in 'AZURE_TENANT_ID', 'AZURE_CLIENT_ID', 'AZURE_CLIENT_SECRET') {
  if (-not [Environment]::GetEnvironmentVariable($variable)) { throw "$variable is not set" }
}

$work = Join-Path ([IO.Path]::GetTempPath()) ("realtimex-link-" + [Guid]::NewGuid())
New-Item -ItemType Directory -Force -Path $work, $Out | Out-Null
$Out = (Resolve-Path $Out).Path
$In = (Resolve-Path $In).Path
$tree = Join-Path $In 'tree'
foreach ($file in (Join-Path $In $name), (Join-Path $tree 'packaging\windows\installer.nsi'), (Join-Path $tree 'assets\icon.ico')) {
  if (-not (Test-Path $file)) { throw "$file is missing: the verify job's output (link-unsigned) didn't arrive" }
}
try {
  $payload = Join-Path $work 'payload'
  New-Item -ItemType Directory -Force -Path $payload | Out-Null
  tar -xzf (Join-Path $In $name) -C $payload
  if ($LASTEXITCODE -ne 0) { throw "tar failed" }

  # Azure Trusted Signing, as electron-builder does it.
  try { Install-PackageProvider -Name NuGet -MinimumVersion 2.8.5.201 -Force -Scope CurrentUser | Out-Null } catch {}
  if (-not (Get-Module -ListAvailable -Name TrustedSigning | Where-Object { $_.Version -eq [version]$trustedSigningVersion })) {
    Install-Module -Name TrustedSigning -RequiredVersion $trustedSigningVersion -Force -Repository PSGallery -Scope CurrentUser
  }
  Import-Module -Name TrustedSigning -RequiredVersion $trustedSigningVersion -Force
  function Sign-Files([string[]]$Files) {
    Invoke-TrustedSigning -Endpoint 'https://wus.codesigning.azure.net/' -CodeSigningAccountName 'RealTimeX' `
      -CertificateProfileName 'realtimex-production-profile' -TimestampRfc3161 'http://timestamp.acs.microsoft.com' `
      -TimestampDigest 'SHA256' -FileDigest 'SHA256' -Files ($Files -join ',')
    foreach ($file in $Files) {
      $signature = Get-AuthenticodeSignature $file
      if ($signature.Status -ne 'Valid' -or $signature.SignerCertificate.Subject -notmatch "CN=`"?$([regex]::Escape($publisher))") {
        throw "$file`: signature $($signature.Status), signer $($signature.SignerCertificate.Subject)"
      }
      # Countersigned by the timestamp server: Trusted Signing certificates live a few days, and
      # without a timestamp the signature stops validating when the certificate expires.
      if (-not $signature.TimeStamperCertificate) {
        throw "$file`: signed without a timestamp"
      }
      Write-Host "signed: $(Split-Path -Leaf $file) ($($signature.SignerCertificate.Subject))"
    }
  }
  Sign-Files ($executables | ForEach-Object { Join-Path $payload $_ })

  # The signed archive, under the unsigned one's name (RealTimeX's pin and installer use it).
  Push-Location $payload
  tar -czf (Join-Path $Out $name) *
  if ($LASTEXITCODE -ne 0) { throw "tar failed" }
  Pop-Location

  # The installer, from the release commit's tree.
  $nsis = @(
    "${env:ProgramFiles(x86)}\NSIS\makensis.exe",
    "$env:ProgramFiles\NSIS\makensis.exe"
  ) | Where-Object { Test-Path $_ } | Select-Object -First 1
  if (-not $nsis) {
    # electron-builder's NSIS, where RealTimeX's own installer builds left it on this runner (the
    # runner's service account keeps its cache in SystemTemp).
    $caches = @($env:ELECTRON_BUILDER_CACHE, (Join-Path "$env:LOCALAPPDATA" 'electron-builder\Cache'), 'C:\Windows\SystemTemp\electron-builder-cache') |
      Where-Object { $_ -and (Test-Path (Join-Path $_ 'nsis')) }
    $nsis = $caches | ForEach-Object { Get-ChildItem -Path (Join-Path $_ 'nsis') -Recurse -Filter makensis.exe -ErrorAction SilentlyContinue } |
      Where-Object { $_.FullName -match '\\Bin\\makensis\.exe$' } | Sort-Object FullName -Descending | Select-Object -First 1 -ExpandProperty FullName
  }
  if (-not $nsis) {
    # Else the same build electron-builder downloads (nsis-3.0.4.1), checked against its SHA-512.
    $archive = Join-Path $work 'nsis-3.0.4.1.7z'
    Invoke-WebRequest -UseBasicParsing -Uri 'https://github.com/electron-userland/electron-builder-binaries/releases/download/nsis-3.0.4.1/nsis-3.0.4.1.7z' -OutFile $archive
    $sha512 = [Convert]::ToBase64String([Security.Cryptography.SHA512]::Create().ComputeHash([IO.File]::ReadAllBytes($archive)))
    if ($sha512 -ne 'VKMiizYdmNdJOWpRGz4trl4lD++BvYP2irAXpMilheUP0pc93iKlWAoP843Vlraj8YG19CVn0j+dCo/hURz9+Q==') { throw "nsis-3.0.4.1.7z: unexpected SHA-512" }
    $unpacked = Join-Path $work 'nsis'
    New-Item -ItemType Directory -Force -Path $unpacked | Out-Null
    tar -xf $archive -C $unpacked
    if ($LASTEXITCODE -ne 0) { throw "couldn't unpack nsis-3.0.4.1.7z" }
    $nsis = Get-ChildItem -Path $unpacked -Recurse -Filter makensis.exe | Where-Object { $_.FullName -match '\\Bin\\makensis\.exe$' } |
      Select-Object -First 1 -ExpandProperty FullName
  }
  if (-not $nsis) { throw "NSIS (makensis.exe) is not on this runner" }
  Write-Host "NSIS: $nsis"
  Push-Location (Join-Path $tree 'packaging\windows')
  & $nsis /V2 "/DVERSION=$Version" "/DPAYLOAD=$payload" "/DOUTFILE=$(Join-Path $Out $setup)" installer.nsi
  if ($LASTEXITCODE -ne 0) { throw "makensis failed" }
  Pop-Location
  Sign-Files @(Join-Path $Out $setup)
  Get-ChildItem $Out | Format-Table Name, Length
} finally {
  Remove-Item -Recurse -Force $work -ErrorAction SilentlyContinue
}
