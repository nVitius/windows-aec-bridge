[CmdletBinding()]
param(
    [string] $Version,
    [switch] $SkipBuild
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot "..")).Path
Push-Location $repoRoot

try {
    $metadata = cargo metadata --locked --no-deps --format-version 1 | ConvertFrom-Json
    if ($LASTEXITCODE -ne 0) {
        throw "cargo metadata failed."
    }

    $package = $metadata.packages | Where-Object { $_.name -eq "aec-bridge" } | Select-Object -First 1
    if ($null -eq $package) {
        throw "Could not find the aec-bridge package in Cargo metadata."
    }

    $manifestVersion = [string] $package.version
    if ([string]::IsNullOrWhiteSpace($Version)) {
        $Version = $manifestVersion
    }
    elseif ($Version -ne $manifestVersion) {
        throw "Requested version '$Version' does not match Cargo.toml version '$manifestVersion'."
    }

    if (-not $SkipBuild) {
        cargo build --release --locked --bin aec-bridge --bin aec-bridge-cli
        if ($LASTEXITCODE -ne 0) {
            throw "Release build failed."
        }
    }

    $distDirectory = Join-Path $repoRoot "dist"
    $packageName = "aec-bridge-$Version-windows-x64"
    $stagingDirectory = Join-Path $distDirectory $packageName
    $archivePath = Join-Path $distDirectory "$packageName.zip"
    $archiveChecksumPath = "$archivePath.sha256"

    New-Item -ItemType Directory -Force -Path $distDirectory | Out-Null

    foreach ($outputPath in @($stagingDirectory, $archivePath, $archiveChecksumPath)) {
        if (Test-Path -LiteralPath $outputPath) {
            throw "Release output already exists: $outputPath`nRemove it or move it aside before packaging again."
        }
    }

    New-Item -ItemType Directory -Path $stagingDirectory | Out-Null

    $releaseFiles = @(
        @{ Source = "target\release\aec-bridge.exe"; Name = "aec-bridge.exe" },
        @{ Source = "target\release\aec-bridge-cli.exe"; Name = "aec-bridge-cli.exe" },
        @{ Source = "Check Endpoints.cmd"; Name = "Check Endpoints.cmd" },
        @{ Source = "Cargo.lock"; Name = "Cargo.lock" },
        @{ Source = "LICENSE"; Name = "LICENSE" },
        @{ Source = "README.md"; Name = "README.md" },
        @{ Source = "THIRD_PARTY_LICENSES.html"; Name = "THIRD_PARTY_LICENSES.html" },
        @{ Source = "THIRD_PARTY_NOTICES.md"; Name = "THIRD_PARTY_NOTICES.md" }
    )

    foreach ($releaseFile in $releaseFiles) {
        $sourcePath = Join-Path $repoRoot $releaseFile.Source
        if (-not (Test-Path -LiteralPath $sourcePath -PathType Leaf)) {
            throw "Required release file is missing: $sourcePath"
        }
        Copy-Item -LiteralPath $sourcePath -Destination (Join-Path $stagingDirectory $releaseFile.Name)
    }

    $checksums = foreach ($releaseFile in $releaseFiles) {
        $packagedPath = Join-Path $stagingDirectory $releaseFile.Name
        $hash = (Get-FileHash -Algorithm SHA256 -LiteralPath $packagedPath).Hash.ToLowerInvariant()
        "$hash  $($releaseFile.Name)"
    }
    $checksums | Set-Content -LiteralPath (Join-Path $stagingDirectory "SHA256SUMS.txt") -Encoding ascii

    Compress-Archive -LiteralPath $stagingDirectory -DestinationPath $archivePath -CompressionLevel Optimal
    $archiveHash = (Get-FileHash -Algorithm SHA256 -LiteralPath $archivePath).Hash.ToLowerInvariant()
    "$archiveHash  $packageName.zip" |
        Set-Content -LiteralPath $archiveChecksumPath -Encoding ascii

    Write-Output "Created $archivePath"
    Write-Output "Created $archiveChecksumPath"
}
finally {
    Pop-Location
}
