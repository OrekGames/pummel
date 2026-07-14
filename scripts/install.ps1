# Pummel - Secure Installer for Windows
# Discovers the latest stable GitHub Release, verifies the signed checksum
# manifest with minisign, validates the archive SHA256 checksum, and installs
# the platform binary.

$ErrorActionPreference = "Stop"

$pubKey = "RWQxie7dcHNLULOnZ3qGIGV5IQHhCs5u48Py3qrbCbGUZ3F6PrHyTCrF"
$scratchDir = Join-Path ([System.IO.Path]::GetTempPath()) ("pummel-install-" + [Guid]::NewGuid().ToString("N"))

function Get-EnvOrDefault {
    param(
        [Parameter(Mandatory = $true)][string]$Name,
        [Parameter(Mandatory = $true)][string]$Default
    )

    $value = [Environment]::GetEnvironmentVariable($Name)
    if ([string]::IsNullOrWhiteSpace($value)) {
        return $Default
    }
    return $value
}

function Normalize-Version {
    param([Parameter(Mandatory = $true)][string]$Version)

    $trimmed = $Version.Trim()
    if ($trimmed -match '^[0-9]+\.[0-9]+\.[0-9]+$') {
        return "v$trimmed"
    }
    if ($trimmed -match '^v[0-9]+\.[0-9]+\.[0-9]+$') {
        return $trimmed
    }
    throw "Unsupported version '$Version'. Expected MAJOR.MINOR.PATCH or vMAJOR.MINOR.PATCH"
}

function Get-WindowsTarget {
    $effectiveArch = if (-not [string]::IsNullOrWhiteSpace($env:PROCESSOR_ARCHITEW6432)) {
        $env:PROCESSOR_ARCHITEW6432
    } else {
        $env:PROCESSOR_ARCHITECTURE
    }

    if ([string]::IsNullOrWhiteSpace($effectiveArch)) {
        $effectiveArch = [System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture.ToString()
    }

    switch -Regex ($effectiveArch) {
        '^(AMD64|x86_64|X64)$' {
            return [PSCustomObject]@{
                Target = "x86_64-pc-windows-msvc"
                ArchiveExt = "zip"
                EffectiveArch = $effectiveArch
            }
        }
        default {
            throw "Unsupported Windows architecture '$effectiveArch'. Only x86_64/AMD64 is currently supported."
        }
    }
}

function Get-MinisignPath {
    $command = Get-Command -Name minisign.exe,minisign -ErrorAction SilentlyContinue | Select-Object -First 1
    if (-not $command) {
        throw "minisign is required. Install it with winget, Chocolatey, Scoop, or from a trusted minisign release source, then rerun this installer."
    }
    return $command.Source
}

function Invoke-Download {
    param(
        [Parameter(Mandatory = $true)][string]$Uri,
        [Parameter(Mandatory = $true)][string]$OutFile
    )

    $headers = @{
        Accept = "application/vnd.github+json"
        "X-GitHub-Api-Version" = "2022-11-28"
    }
    Invoke-WebRequest -Uri $Uri -OutFile $OutFile -Headers $headers -UseBasicParsing -ErrorAction Stop | Out-Null
}

function Get-NextPageUrl {
    param([Parameter(Mandatory = $true)]$Response)

    $link = $Response.Headers["Link"]
    if ($null -eq $link) {
        return ""
    }

    $linkValue = if ($link -is [System.Array]) { $link -join "," } else { [string]$link }
    foreach ($part in ($linkValue -split ",")) {
        if ($part -match '<([^>]+)>\s*;\s*rel="next"') {
            return $Matches[1]
        }
    }
    return ""
}

function Find-LatestStableVersion {
    param(
        [Parameter(Mandatory = $true)][string]$GitHubApiBase,
        [Parameter(Mandatory = $true)][string]$Repo
    )

    $uri = "$GitHubApiBase/repos/$Repo/releases?per_page=100&page=1"
    $versions = [System.Collections.Generic.List[object]]::new()

    while (-not [string]::IsNullOrWhiteSpace($uri)) {
        $response = Invoke-WebRequest -Uri $uri -Headers @{
            Accept = "application/vnd.github+json"
            "X-GitHub-Api-Version" = "2022-11-28"
        } -UseBasicParsing -ErrorAction Stop

        $releases = @()
        if (-not [string]::IsNullOrWhiteSpace($response.Content)) {
            $parsed = $response.Content | ConvertFrom-Json -ErrorAction Stop
            if ($null -ne $parsed) {
                $releases = @($parsed)
            }
        }

        foreach ($release in $releases) {
            if ($release.draft -or $release.prerelease) {
                continue
            }
            $tag = [string]$release.tag_name
            if ($tag -match '^v([0-9]+)\.([0-9]+)\.([0-9]+)$') {
                $versions.Add([PSCustomObject]@{
                    Major = [int]$Matches[1]
                    Minor = [int]$Matches[2]
                    Patch = [int]$Matches[3]
                    Tag = $tag
                }) | Out-Null
            }
        }

        $uri = Get-NextPageUrl -Response $response
    }

    if ($versions.Count -eq 0) {
        throw "No stable vMAJOR.MINOR.PATCH releases found on GitHub."
    }

    return ($versions | Sort-Object Major,Minor,Patch | Select-Object -Last 1).Tag
}

function Assert-ManifestSignature {
    param(
        [Parameter(Mandatory = $true)][string]$MinisignPath,
        [Parameter(Mandatory = $true)][string]$ManifestPath,
        [Parameter(Mandatory = $true)][string]$SignaturePath,
        [Parameter(Mandatory = $true)][string]$PublicKey
    )

    Write-Host "Verifying signed checksum manifest with minisign..."
    & $MinisignPath -V -P $PublicKey -m $ManifestPath -x $SignaturePath
    if ($LASTEXITCODE -ne 0) {
        throw "Signature verification failed for checksums-sha256.txt."
    }
}

function Get-ExpectedHash {
    param(
        [Parameter(Mandatory = $true)][string]$ManifestPath,
        [Parameter(Mandatory = $true)][string]$ArchiveName
    )

    foreach ($line in Get-Content -LiteralPath $ManifestPath) {
        if ($line -match '^([a-fA-F0-9]{64})\s+(.+)$' -and $Matches[2].Trim() -eq $ArchiveName) {
            return $Matches[1].ToLowerInvariant()
        }
    }

    throw "Archive $ArchiveName not found in verified checksums-sha256.txt."
}

function Assert-ArchiveChecksum {
    param(
        [Parameter(Mandatory = $true)][string]$ManifestPath,
        [Parameter(Mandatory = $true)][string]$ArchiveName,
        [Parameter(Mandatory = $true)][string]$ArchivePath
    )

    $expectedHash = Get-ExpectedHash -ManifestPath $ManifestPath -ArchiveName $ArchiveName
    $actualHash = (Get-FileHash -LiteralPath $ArchivePath -Algorithm SHA256).Hash.ToLowerInvariant()

    if ($expectedHash -ne $actualHash) {
        throw "SHA256 checksum mismatch for $ArchiveName. Expected: $expectedHash Actual: $actualHash"
    }
}

function Assert-SafeZipEntries {
    param([Parameter(Mandatory = $true)][string]$ArchivePath)

    Add-Type -AssemblyName System.IO.Compression.FileSystem
    $archive = [System.IO.Compression.ZipFile]::OpenRead($ArchivePath)
    try {
        $entries = @($archive.Entries | ForEach-Object { $_.FullName -replace '\\', '/' })
        if ($entries.Count -ne 1 -or $entries[0] -ne "pummel.exe") {
            throw "Archive must contain exactly one root-level pummel.exe entry. Found: $($entries -join ', ')"
        }
    } finally {
        $archive.Dispose()
    }
}

function Install-PummelBinary {
    param(
        [Parameter(Mandatory = $true)][string]$ArchivePath,
        [Parameter(Mandatory = $true)][string]$ScratchDir,
        [Parameter(Mandatory = $false)][string]$InstallDirOverride
    )

    Write-Host "Extracting Pummel binary..."
    Assert-SafeZipEntries -ArchivePath $ArchivePath

    $extractDir = Join-Path $ScratchDir "extract"
    New-Item -ItemType Directory -Force -Path $extractDir | Out-Null
    Expand-Archive -Path $ArchivePath -DestinationPath $extractDir -Force

    $binarySource = Join-Path $extractDir "pummel.exe"
    if (-not (Test-Path -LiteralPath $binarySource)) {
        throw "Extracted archive did not contain pummel.exe at the archive root."
    }

    $resolved = (Resolve-Path -LiteralPath $binarySource).Path
    $extractRoot = (Resolve-Path -LiteralPath $extractDir).Path
    if (-not $resolved.StartsWith($extractRoot, [System.StringComparison]::OrdinalIgnoreCase)) {
        throw "Refusing to install binary outside extract directory: $resolved"
    }

    $installDir = if (-not [string]::IsNullOrWhiteSpace($InstallDirOverride)) {
        $InstallDirOverride
    } else {
        Join-Path $HOME ".local\bin"
    }

    New-Item -ItemType Directory -Force -Path $installDir | Out-Null
    $binaryDest = Join-Path $installDir "pummel.exe"
    Copy-Item -Path $binarySource -Destination $binaryDest -Force

    Write-Host "Pummel installed successfully to $binaryDest."
    Write-Host "Please ensure '$installDir' is in your PATH environment variable."
    Write-Host "You can check if it works by running: pummel --version"
}

try {
    New-Item -ItemType Directory -Force -Path $scratchDir | Out-Null

    $gitHubApiBase = (Get-EnvOrDefault -Name "PUMMEL_GITHUB_API_BASE" -Default "https://api.github.com").TrimEnd('/')
    $repo = Get-EnvOrDefault -Name "PUMMEL_REPO" -Default "OrekGames/pummel"
    $requestedVersion = [Environment]::GetEnvironmentVariable("PUMMEL_VERSION")
    $installDirOverride = [Environment]::GetEnvironmentVariable("PUMMEL_INSTALL_DIR")
    $downloadBaseOverride = [Environment]::GetEnvironmentVariable("PUMMEL_DOWNLOAD_BASE")

    $targetInfo = Get-WindowsTarget
    $minisignPath = Get-MinisignPath

    if (-not [string]::IsNullOrWhiteSpace($requestedVersion)) {
        $version = Normalize-Version -Version $requestedVersion
        Write-Host "Using requested Pummel version: $version"
    } else {
        Write-Host "Discovering latest stable Pummel version from GitHub Releases..."
        $version = Find-LatestStableVersion -GitHubApiBase $gitHubApiBase -Repo $repo
        Write-Host "Found latest stable Pummel version: $version"
    }

    $downloadBase = if (-not [string]::IsNullOrWhiteSpace($downloadBaseOverride)) {
        $downloadBaseOverride.TrimEnd('/')
    } else {
        "https://github.com/$repo/releases/download/$version"
    }

    $manifestPath = Join-Path $scratchDir "checksums-sha256.txt"
    $signaturePath = Join-Path $scratchDir "checksums-sha256.txt.minisig"
    $archiveName = "pummel-$version-$($targetInfo.Target).$($targetInfo.ArchiveExt)"
    $archivePath = Join-Path $scratchDir $archiveName

    Write-Host "Downloading signed checksum manifest..."
    Invoke-Download -Uri "$downloadBase/checksums-sha256.txt" -OutFile $manifestPath
    Invoke-Download -Uri "$downloadBase/checksums-sha256.txt.minisig" -OutFile $signaturePath
    Assert-ManifestSignature -MinisignPath $minisignPath -ManifestPath $manifestPath -SignaturePath $signaturePath -PublicKey $pubKey

    Write-Host "Downloading Pummel archive: $archiveName..."
    Invoke-Download -Uri "$downloadBase/$archiveName" -OutFile $archivePath

    Write-Host "Verifying archive checksum..."
    Assert-ArchiveChecksum -ManifestPath $manifestPath -ArchiveName $archiveName -ArchivePath $archivePath

    Install-PummelBinary -ArchivePath $archivePath -ScratchDir $scratchDir -InstallDirOverride $installDirOverride
} catch {
    Write-Error $_
    exit 1
} finally {
    if (Test-Path -LiteralPath $scratchDir) {
        Remove-Item -Path $scratchDir -Recurse -Force -ErrorAction SilentlyContinue | Out-Null
    }
}
