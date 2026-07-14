# Offline PowerShell installer harness: checksum verification and zip safety.
# Invoked from CI on windows-latest. Does not contact GitHub.

$ErrorActionPreference = "Stop"

$Root = (Resolve-Path (Join-Path $PSScriptRoot "..\..")).Path
$Installer = Join-Path $Root "scripts\install.ps1"
$FixtureRoot = Join-Path ([System.IO.Path]::GetTempPath()) ("pummel-ps1-harness-" + [Guid]::NewGuid().ToString("N"))
$Pass = 0
$Fail = 0
$HttpListener = $null

function Write-Info([string]$Message) {
    Write-Host "==> $Message"
}

function Pass([string]$Message) {
    $script:Pass++
    Write-Host "PASS: $Message"
}

function Fail([string]$Message) {
    $script:Fail++
    Write-Host "FAIL: $Message" -ForegroundColor Red
}

function New-FixtureDir([string]$Name) {
    $path = Join-Path $FixtureRoot $Name
    New-Item -ItemType Directory -Force -Path $path | Out-Null
    return $path
}

function New-GoodZip {
    param(
        [Parameter(Mandatory = $true)][string]$Dir,
        [Parameter(Mandatory = $true)][string]$Version,
        [Parameter(Mandatory = $true)][string]$Target
    )

    $payload = Join-Path $Dir "payload"
    New-Item -ItemType Directory -Force -Path $payload | Out-Null
    $exe = Join-Path $payload "pummel.exe"
    Set-Content -LiteralPath $exe -Value "pummel-fixture" -NoNewline -Encoding Ascii

    $archiveName = "pummel-$Version-$Target.zip"
    $archivePath = Join-Path $Dir $archiveName
    if (Test-Path -LiteralPath $archivePath) {
        Remove-Item -LiteralPath $archivePath -Force
    }
    Compress-Archive -Path $exe -DestinationPath $archivePath -Force

    $hash = (Get-FileHash -LiteralPath $archivePath -Algorithm SHA256).Hash.ToLowerInvariant()
    Set-Content -LiteralPath (Join-Path $Dir "checksums-sha256.txt") -Value "$hash  $archiveName" -Encoding Ascii
    return $archiveName
}

function Start-LocalHttp([string]$RootDir) {
    $port = Get-Random -Minimum 18000 -Maximum 28000
    $listener = New-Object System.Net.HttpListener
    $listener.Prefixes.Add("http://127.0.0.1:$port/")
    $listener.Start()

    $runspace = [runspacefactory]::CreateRunspace()
    $runspace.Open()
    $ps = [powershell]::Create()
    $ps.Runspace = $runspace
    [void]$ps.AddScript({
        param($Listener, $RootDir)
        while ($Listener.IsListening) {
            try {
                $context = $Listener.GetContext()
            } catch {
                break
            }
            $rel = [Uri]::UnescapeDataString($context.Request.Url.AbsolutePath.TrimStart('/'))
            $rel = $rel -replace '/', [IO.Path]::DirectorySeparatorChar
            $path = [IO.Path]::GetFullPath((Join-Path $RootDir $rel))
            $rootFull = [IO.Path]::GetFullPath($RootDir)
            if (-not $path.StartsWith($rootFull, [StringComparison]::OrdinalIgnoreCase)) {
                $context.Response.StatusCode = 400
                $context.Response.Close()
                continue
            }
            if (Test-Path -LiteralPath $path -PathType Leaf) {
                $bytes = [IO.File]::ReadAllBytes($path)
                $context.Response.StatusCode = 200
                $context.Response.ContentLength64 = $bytes.Length
                $context.Response.OutputStream.Write($bytes, 0, $bytes.Length)
            } else {
                $context.Response.StatusCode = 404
            }
            $context.Response.OutputStream.Close()
        }
    }).AddArgument($listener).AddArgument($RootDir) | Out-Null
    $handle = $ps.BeginInvoke()
    Start-Sleep -Milliseconds 200
    return @{
        Listener = $listener
        Port = $port
        PowerShell = $ps
        Handle = $handle
        Runspace = $runspace
    }
}

function Stop-LocalHttp($State) {
    if ($null -eq $State) { return }
    if ($null -ne $State.Listener) {
        try { $State.Listener.Stop() } catch {}
        try { $State.Listener.Close() } catch {}
    }
    if ($null -ne $State.PowerShell) {
        try { $State.PowerShell.Stop() } catch {}
        try { $State.PowerShell.Dispose() } catch {}
    }
    if ($null -ne $State.Runspace) {
        try { $State.Runspace.Close() } catch {}
        try { $State.Runspace.Dispose() } catch {}
    }
}

function Invoke-Installer {
    param(
        [Parameter(Mandatory = $true)][string]$DownloadBase,
        [Parameter(Mandatory = $true)][string]$InstallDir,
        [Parameter(Mandatory = $true)][string]$Version
    )

    $env:PUMMEL_DOWNLOAD_BASE = $DownloadBase
    $env:PUMMEL_INSTALL_DIR = $InstallDir
    $env:PUMMEL_VERSION = $Version
    $output = & powershell -NoProfile -ExecutionPolicy Bypass -File $Installer 2>&1
    $code = $LASTEXITCODE
    return [PSCustomObject]@{
        ExitCode = $code
        Output = ($output | Out-String)
    }
}

try {
    New-Item -ItemType Directory -Force -Path $FixtureRoot | Out-Null
    $Target = "x86_64-pc-windows-msvc"
    $Version = "v0.1.0"

    Write-Info "Parse install.ps1"
    $tokens = $null
    $errors = $null
    [void][System.Management.Automation.Language.Parser]::ParseFile($Installer, [ref]$tokens, [ref]$errors)
    if ($errors -and $errors.Count -gt 0) {
        Fail ("install.ps1 parse errors: " + ($errors | ForEach-Object { $_.ToString() } | Out-String))
    } else {
        Pass "install.ps1 parses"
    }

    Write-Info "Happy path"
    $good = New-FixtureDir "good"
    $versionDir = Join-Path $good $Version
    New-Item -ItemType Directory -Force -Path $versionDir | Out-Null
    [void](New-GoodZip -Dir $versionDir -Version $Version -Target $Target)
    $http = Start-LocalHttp -RootDir $good
    $installDir = New-FixtureDir "install-good"
    $result = Invoke-Installer -DownloadBase "http://127.0.0.1:$($http.Port)" -InstallDir $installDir -Version $Version
    Stop-LocalHttp $http
    if ($result.ExitCode -eq 0 -and (Test-Path (Join-Path $installDir "pummel.exe"))) {
        Pass "happy path install"
    } else {
        Fail "happy path failed (exit=$($result.ExitCode))"
        Write-Host $result.Output
    }

    Write-Info "Bad checksum"
    $bad = New-FixtureDir "bad"
    $versionDir = Join-Path $bad $Version
    New-Item -ItemType Directory -Force -Path $versionDir | Out-Null
    $archiveName = New-GoodZip -Dir $versionDir -Version $Version -Target $Target
    Set-Content -LiteralPath (Join-Path $versionDir "checksums-sha256.txt") `
        -Value (("0" * 64) + "  $archiveName") -Encoding Ascii
    $http = Start-LocalHttp -RootDir $bad
    $installDir = New-FixtureDir "install-bad"
    $result = Invoke-Installer -DownloadBase "http://127.0.0.1:$($http.Port)" -InstallDir $installDir -Version $Version
    Stop-LocalHttp $http
    if ($result.ExitCode -ne 0) {
        Pass "bad checksum rejected"
    } else {
        Fail "bad checksum should have failed"
    }

    Write-Info "Unsafe zip member"
    $unsafe = New-FixtureDir "unsafe"
    $versionDir = Join-Path $unsafe $Version
    New-Item -ItemType Directory -Force -Path $versionDir | Out-Null
    $payload = Join-Path $versionDir "payload"
    New-Item -ItemType Directory -Force -Path $payload | Out-Null
    $evil = Join-Path $payload "evil.exe"
    Set-Content -LiteralPath $evil -Value "evil" -NoNewline -Encoding Ascii
    $archiveName = "pummel-$Version-$Target.zip"
    $archivePath = Join-Path $versionDir $archiveName
    Compress-Archive -Path $evil -DestinationPath $archivePath -Force
    $hash = (Get-FileHash -LiteralPath $archivePath -Algorithm SHA256).Hash.ToLowerInvariant()
    Set-Content -LiteralPath (Join-Path $versionDir "checksums-sha256.txt") -Value "$hash  $archiveName" -Encoding Ascii
    $http = Start-LocalHttp -RootDir $unsafe
    $installDir = New-FixtureDir "install-unsafe"
    $result = Invoke-Installer -DownloadBase "http://127.0.0.1:$($http.Port)" -InstallDir $installDir -Version $Version
    Stop-LocalHttp $http
    if ($result.ExitCode -ne 0) {
        Pass "unsafe zip member rejected"
    } else {
        Fail "unsafe zip should have failed"
    }

    Write-Host ""
    Write-Host "Harness summary: $Pass passed, $Fail failed"
    if ($Fail -ne 0 -or $Pass -lt 3) {
        exit 1
    }
} finally {
    if (Test-Path -LiteralPath $FixtureRoot) {
        Remove-Item -LiteralPath $FixtureRoot -Recurse -Force -ErrorAction SilentlyContinue
    }
}
