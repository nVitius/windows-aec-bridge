[CmdletBinding()]
param(
    [string] $OutputPath
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

$cargoAboutVersion = "0.9.2"
$cargoAboutSha256 = "1c03e5890238562497c2d89a3b75b02560af349c1fc3e713d3284f532a5cd748"
$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot "..")).Path

if ([string]::IsNullOrWhiteSpace($OutputPath)) {
    $OutputPath = Join-Path $repoRoot "THIRD_PARTY_LICENSES.html"
}
elseif (-not [System.IO.Path]::IsPathRooted($OutputPath)) {
    $OutputPath = Join-Path $repoRoot $OutputPath
}
$OutputPath = [System.IO.Path]::GetFullPath($OutputPath)

$systemTemp = [System.IO.Path]::GetFullPath([System.IO.Path]::GetTempPath())
$temporaryDirectory = Join-Path $systemTemp "aec-bridge-cargo-about-$([guid]::NewGuid().ToString('N'))"
$temporaryDirectory = [System.IO.Path]::GetFullPath($temporaryDirectory)
if (-not $temporaryDirectory.StartsWith($systemTemp, [System.StringComparison]::OrdinalIgnoreCase)) {
    throw "Refusing to use a temporary directory outside the system temporary directory."
}

$archiveName = "cargo-about-$cargoAboutVersion-x86_64-pc-windows-msvc.tar.gz"
$archivePath = Join-Path $temporaryDirectory $archiveName
$downloadUri = "https://github.com/EmbarkStudios/cargo-about/releases/download/$cargoAboutVersion/$archiveName"

New-Item -ItemType Directory -Path $temporaryDirectory | Out-Null
Push-Location $repoRoot

try {
    Invoke-WebRequest -Uri $downloadUri -OutFile $archivePath
    $actualSha256 = (Get-FileHash -LiteralPath $archivePath -Algorithm SHA256).Hash.ToLowerInvariant()
    if ($actualSha256 -ne $cargoAboutSha256) {
        throw "cargo-about checksum mismatch: expected $cargoAboutSha256, got $actualSha256"
    }

    tar.exe -xzf $archivePath -C $temporaryDirectory
    if ($LASTEXITCODE -ne 0) {
        throw "Could not extract the verified cargo-about archive."
    }

    $cargoAbout = Get-ChildItem -LiteralPath $temporaryDirectory -Recurse -File -Filter "cargo-about.exe" |
        Select-Object -First 1
    if ($null -eq $cargoAbout) {
        throw "cargo-about.exe was not found in the verified archive."
    }

    & $cargoAbout.FullName generate --locked --fail --config about.toml --output-file $OutputPath about.hbs
    if ($LASTEXITCODE -ne 0) {
        throw "cargo-about failed to generate the third-party license bundle."
    }

    Write-Output "Generated $OutputPath"
}
finally {
    Pop-Location
    if (Test-Path -LiteralPath $temporaryDirectory) {
        Remove-Item -LiteralPath $temporaryDirectory -Recurse -Force
    }
}
