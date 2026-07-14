# Pummel - Installer for Windows
# Discovers the latest stable GitHub Release, verifies the archive SHA256
# checksum against checksums-sha256.txt, and installs the platform binary.

$ErrorActionPreference = "Stop"

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

function Invoke-Download {
    param(
        [Parameter(Mandatory = $true)][string]$Uri,
        [Parameter(Mandatory = $true)][string]$OutFile
    )

    $headers = @{
        Accept = "application/vnd.github+json"
        "X-GitHub-Api-Version" = "2022-11-28"
    }

    $attempt = 0
    while ($true) {
        $attempt += 1
        try {
            Invoke-WebRequest -Uri $Uri -OutFile $OutFile -Headers $headers -UseBasicParsing -ErrorAction Stop | Out-Null
            return
        } catch {
            if ($attempt -ge 3) {
                throw
            }
            Start-Sleep -Seconds (2 * $attempt)
        }
    }
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
        $attempt = 0
        while ($true) {
            $attempt += 1
            try {
                $response = Invoke-WebRequest -Uri $uri -Headers @{
                    Accept = "application/vnd.github+json"
                    "X-GitHub-Api-Version" = "2022-11-28"
                } -UseBasicParsing -ErrorAction Stop
                break
            } catch {
                if ($attempt -ge 3) {
                    throw
                }
                Start-Sleep -Seconds (2 * $attempt)
            }
        }

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
                    Major = [bigint]$Matches[1]
                    Minor = [bigint]$Matches[2]
                    Patch = [bigint]$Matches[3]
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

    throw "Archive $ArchiveName not found in checksums-sha256.txt."
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
        $entries = @($archive.Entries)
        if ($entries.Count -ne 1) {
            throw "Archive must contain exactly one entry. Found: $($entries.Count)"
        }
        $name = ($entries[0].FullName -replace '\\', '/')
        if ($name -ne "pummel.exe") {
            throw "Archive must contain exactly one root-level pummel.exe entry. Found: $name"
        }
        if ($name.Contains("..") -or $name.StartsWith("/")) {
            throw "Refusing unsafe zip entry path: $name"
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

    if (-not [string]::IsNullOrWhiteSpace($requestedVersion)) {
        $version = Normalize-Version -Version $requestedVersion
        Write-Host "Using requested Pummel version: $version"
    } else {
        Write-Host "Discovering latest stable Pummel version from GitHub Releases..."
        $version = Find-LatestStableVersion -GitHubApiBase $gitHubApiBase -Repo $repo
        Write-Host "Found latest stable Pummel version: $version"
    }

    # Same contract as install.sh: override is a root URL; version is appended.
    $downloadBase = if (-not [string]::IsNullOrWhiteSpace($downloadBaseOverride)) {
        ($downloadBaseOverride.TrimEnd('/')) + "/" + $version
    } else {
        "https://github.com/$repo/releases/download/$version"
    }

    $manifestPath = Join-Path $scratchDir "checksums-sha256.txt"
    $archiveName = "pummel-$version-$($targetInfo.Target).$($targetInfo.ArchiveExt)"
    $archivePath = Join-Path $scratchDir $archiveName

    Write-Host "Downloading checksum manifest..."
    Invoke-Download -Uri "$downloadBase/checksums-sha256.txt" -OutFile $manifestPath

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
