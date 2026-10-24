# Sign RealTimeX Link for Windows, on RealTimeX's Windows release runner (the GitLab `sign-win`
# job): the unsigned archive from the draft release (checked against its SHA256SUMS) in; out, the
# archive (same name) with the executables signed by Azure Trusted Signing, and
# RealTimeX-Link-Setup-<version>.exe, Link's NSIS installer (packaging/windows/installer.nsi)
# around them, signed. Signing is the one RealTimeX's electron-builder uses: the TrustedSigning
# module, RealTimeX's account and profile, AZURE_TENANT_ID / AZURE_CLIENT_ID / AZURE_CLIENT_SECRET.
# The draft release is read with GITHUB_LINK_TOKEN.
param(
  [Parameter(Mandatory = $true)][string]$Version,
  [Parameter(Mandatory = $true)][string]$Out
)
$ErrorActionPreference = 'Stop'
$repository = if ($env:GITHUB_REPOSITORY) { $env:GITHUB_REPOSITORY } else { 'therealtimex/realtimex-link' }
$name = 'realtimex-link-windows-x86_64.tar.gz'
$setup = "RealTimeX-Link-Setup-$Version.exe"
$publisher = 'Dang Le'
$executables = @('rtx-linkd.exe', 'rtx-link.exe', 'rtx-control.exe')

foreach ($variable in 'AZURE_TENANT_ID', 'AZURE_CLIENT_ID', 'AZURE_CLIENT_SECRET', 'GITHUB_LINK_TOKEN') {
  if (-not [Environment]::GetEnvironmentVariable($variable)) { throw "$variable is not set" }
}

$work = Join-Path ([IO.Path]::GetTempPath()) ("realtimex-link-" + [Guid]::NewGuid())
New-Item -ItemType Directory -Force -Path $work, $Out | Out-Null
$Out = (Resolve-Path $Out).Path
try {
  # The unsigned build, only if it is the one the release lists. A draft's assets come through
  # the API, with the token.
  $headers = @{ Authorization = "Bearer $env:GITHUB_LINK_TOKEN"; 'X-GitHub-Api-Version' = '2022-11-28'; 'User-Agent' = 'realtimex-link-gitlab-ci' }
  $release = (Invoke-RestMethod -Headers $headers -Uri "https://api.github.com/repos/$repository/releases?per_page=50") |
    Where-Object { $_.tag_name -eq "v$Version" } | Select-Object -First 1
  if (-not $release) { throw "$repository has no release v$Version" }
  foreach ($file in $name, 'SHA256SUMS') {
    $asset = $release.assets | Where-Object { $_.name -eq $file } | Select-Object -First 1
    if (-not $asset) { throw "release v$Version has no $file" }
    Invoke-WebRequest -UseBasicParsing -Headers ($headers + @{ Accept = 'application/octet-stream' }) -Uri $asset.url -OutFile (Join-Path $work $file)
  }
  $expected = (Get-Content (Join-Path $work 'SHA256SUMS') | Where-Object { $_ -match " $([regex]::Escape($name))$" }) -split '\s+' | Select-Object -First 1
  $actual = (Get-FileHash -Algorithm SHA256 (Join-Path $work $name)).Hash.ToLower()
  if (-not $expected -or $actual -ne $expected) { throw "$name`: SHA-256 $actual, but the release lists '$expected'" }
  $payload = Join-Path $work 'payload'
  New-Item -ItemType Directory -Force -Path $payload | Out-Null
  tar -xzf (Join-Path $work $name) -C $payload
  if ($LASTEXITCODE -ne 0) { throw "tar failed" }

  # Azure Trusted Signing, as electron-builder does it.
  try { Install-PackageProvider -Name NuGet -MinimumVersion 2.8.5.201 -Force -Scope CurrentUser | Out-Null } catch {}
  Install-Module -Name TrustedSigning -MinimumVersion 0.5.0 -Force -Repository PSGallery -Scope CurrentUser
  function Sign-Files([string[]]$Files) {
    Invoke-TrustedSigning -Endpoint 'https://wus.codesigning.azure.net/' -CodeSigningAccountName 'RealTimeX' `
      -CertificateProfileName 'realtimex-production-profile' -TimestampRfc3161 'http://timestamp.acs.microsoft.com' `
      -TimestampDigest 'SHA256' -FileDigest 'SHA256' -Files ($Files -join ',')
    foreach ($file in $Files) {
      $signature = Get-AuthenticodeSignature $file
      if ($signature.Status -ne 'Valid' -or $signature.SignerCertificate.Subject -notmatch "CN=`"?$([regex]::Escape($publisher))") {
        throw "$file`: signature $($signature.Status), signer $($signature.SignerCertificate.Subject)"
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

  # The installer, from this checkout (the release's tag).
  $source = (Get-Location).Path
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
  Push-Location (Join-Path $source 'packaging\windows')
  & $nsis /V2 "/DVERSION=$Version" "/DPAYLOAD=$payload" "/DOUTFILE=$(Join-Path $Out $setup)" installer.nsi
  if ($LASTEXITCODE -ne 0) { throw "makensis failed" }
  Pop-Location
  Sign-Files @(Join-Path $Out $setup)
  Get-ChildItem $Out | Format-Table Name, Length
} finally {
  Remove-Item -Recurse -Force $work -ErrorAction SilentlyContinue
}
