<#
  mcp-agent-mail installer (Windows)

  One-liner:
    iwr -useb "https://raw.githubusercontent.com/Dicklesworthstone/mcp_agent_mail_rust/main/install.ps1?$(Get-Random)" | iex

  Options:
    -Version vX.Y.Z   Install a specific release tag (default: latest)
    -Dest PATH        Install directory (default: %LOCALAPPDATA%\Programs\mcp-agent-mail)
    -Force            Reinstall even if the same version is already present
    -NoVerify         Skip checksum verification (not recommended)
    -Verify           Force checksum verification (default behavior)
#>

[CmdletBinding()]
param(
    [string]$Version = "",
    [string]$Dest = "",
    [switch]$Force,
    [switch]$NoVerify,
    [switch]$Verify
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

$Owner = "Dicklesworthstone"
$Repo = "mcp_agent_mail_rust"
$Target = "x86_64-pc-windows-msvc"
$AssetName = "mcp-agent-mail-$Target.zip"
$DefaultDest = Join-Path $env:LOCALAPPDATA "Programs\mcp-agent-mail"
$IssuesUrl = "https://github.com/$Owner/$Repo/issues"
$ReleasesUrl = "https://github.com/$Owner/$Repo/releases"

if ([string]::IsNullOrWhiteSpace($Dest)) {
    $Dest = $DefaultDest
}
$Dest = [System.IO.Path]::GetFullPath($Dest)

if ([System.Environment]::OSVersion.Platform -ne [System.PlatformID]::Win32NT) {
    throw "install.ps1 is only supported on Windows. On Linux/macOS use install.sh: curl -fsSL https://raw.githubusercontent.com/$Owner/$Repo/main/install.sh | bash"
}

if ($Verify -and $NoVerify) {
    throw "Cannot combine -Verify and -NoVerify. Choose one, or omit both to use default verification behavior."
}

$ShouldVerifyChecksum = if ($NoVerify) { $false } else { $true }

if ([Net.ServicePointManager]::SecurityProtocol -band [Net.SecurityProtocolType]::Tls12) {
    # no-op: TLS 1.2 already enabled
} else {
    [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12
}

function Write-Info {
    param([string]$Message)
    Write-Host "-> $Message" -ForegroundColor Cyan
}

function Write-Ok {
    param([string]$Message)
    Write-Host "ok $Message" -ForegroundColor Green
}

function Write-WarnText {
    param([string]$Message)
    Write-Host "!! $Message" -ForegroundColor Yellow
}

function Normalize-Version {
    param([string]$RawVersion)
    if ([string]::IsNullOrWhiteSpace($RawVersion)) {
        return ""
    }
    $trimmed = $RawVersion.Trim()
    $semverMatch = [regex]::Match(
        $trimmed,
        "(?<!\d)v?(\d+\.\d+\.\d+(?:-[0-9A-Za-z\.-]+)?(?:\+[0-9A-Za-z\.-]+)?)"
    )
    if ($semverMatch.Success) {
        return $semverMatch.Groups[1].Value
    }
    if ($trimmed.StartsWith("v", [System.StringComparison]::OrdinalIgnoreCase)) {
        return $trimmed.Substring(1)
    }
    return $trimmed
}

function Resolve-Version {
    param([string]$RequestedVersion)
    if (-not [string]::IsNullOrWhiteSpace($RequestedVersion)) {
        return $RequestedVersion.Trim()
    }

    Write-Info "Resolving latest release version..."
    $latestUrl = "https://api.github.com/repos/$Owner/$Repo/releases/latest"
    $headers = @{ "User-Agent" = "mcp-agent-mail-install.ps1" }
    $response = Invoke-RestMethod -Method Get -Uri $latestUrl -Headers $headers

    if ($null -eq $response -or [string]::IsNullOrWhiteSpace($response.tag_name)) {
        throw "Unable to resolve latest release tag from $latestUrl. Check network/GitHub API access, or pass -Version vX.Y.Z explicitly."
    }

    return [string]$response.tag_name
}

function Get-InstalledVersion {
    param([string]$InstallDir)
    $amExe = Join-Path $InstallDir "am.exe"
    if (-not (Test-Path -LiteralPath $amExe)) {
        return ""
    }

    try {
        $line = (& $amExe --version 2>$null | Select-Object -First 1)
        if ($null -eq $line) {
            return ""
        }
        return ([string]$line).Trim()
    } catch {
        return ""
    }
}

function Ensure-UserPathEntry {
    param([string]$InstallDir)
    $currentPath = [Environment]::GetEnvironmentVariable("Path", "User")
    if ($null -eq $currentPath) {
        $currentPath = ""
    }

    $parts = if ($currentPath.Length -gt 0) {
        $currentPath.Split(";") | Where-Object { -not [string]::IsNullOrWhiteSpace($_) }
    } else {
        @()
    }

    $normalizedInstallDir = $InstallDir.TrimEnd("\").ToLowerInvariant()
    $filtered = @()
    foreach ($entry in $parts) {
        if ($entry.TrimEnd("\").ToLowerInvariant() -eq $normalizedInstallDir) {
            continue
        }
        $filtered += $entry
    }

    $newParts = @($InstallDir) + $filtered
    $newPath = ($newParts -join ";")
    $changed = ($newPath -ne $currentPath)
    [Environment]::SetEnvironmentVariable("Path", $newPath, "User")

    $machinePath = [Environment]::GetEnvironmentVariable("Path", "Machine")
    $processParts = @($InstallDir)
    if (-not [string]::IsNullOrWhiteSpace($machinePath)) {
        $processParts += ($machinePath.Split(";") | Where-Object { -not [string]::IsNullOrWhiteSpace($_) })
    }
    $processParts += $filtered
    $env:Path = ($processParts -join ";")
    return $changed
}

function Download-File {
    param(
        [string]$Url,
        [string]$OutFile
    )
    $headers = @{ "User-Agent" = "mcp-agent-mail-install.ps1" }
    $invokeParams = @{
        Uri     = $Url
        OutFile = $OutFile
        Headers = $headers
    }
    if ((Get-Command Invoke-WebRequest).Parameters.ContainsKey("UseBasicParsing")) {
        $invokeParams.UseBasicParsing = $true
    }
    Invoke-WebRequest @invokeParams
}

function Get-Sha256Hex {
    param([string]$FilePath)
    if (-not (Test-Path -LiteralPath $FilePath)) {
        throw "SHA256 source file not found: $FilePath. Re-run installer to re-download artifacts, or verify the custom path exists."
    }
    return (Get-FileHash -LiteralPath $FilePath -Algorithm SHA256).Hash.ToLowerInvariant()
}

function Parse-ChecksumHex {
    param([string]$ChecksumText)
    if ([string]::IsNullOrWhiteSpace($ChecksumText)) {
        throw "Checksum text is empty. Re-download the checksum file; use -NoVerify only for trusted local artifacts."
    }
    $match = [regex]::Match($ChecksumText, "(?i)\b([a-f0-9]{64})\b")
    if (-not $match.Success) {
        throw "Could not parse SHA256 checksum from text. Ensure the checksum file contains a 64-character SHA256 hex digest."
    }
    return $match.Groups[1].Value.ToLowerInvariant()
}

function Resolve-ChecksumText {
    param(
        [string]$AssetUrl,
        [string]$AssetName,
        [string]$WorkDir
    )

    $checksumPath = Join-Path $WorkDir "$AssetName.sha256"
    $checksumUrl = "$AssetUrl.sha256"
    try {
        Write-Info "Downloading checksum $checksumUrl"
        Download-File -Url $checksumUrl -OutFile $checksumPath
        return (Get-Content -LiteralPath $checksumPath -Raw)
    } catch {
        $sha256sumsUrl = [regex]::Replace($AssetUrl, "/$([regex]::Escape($AssetName))$", "/SHA256SUMS")
        $sha256sumsPath = Join-Path $WorkDir "SHA256SUMS"
        Write-WarnText "Per-asset checksum unavailable; falling back to $sha256sumsUrl"
        Download-File -Url $sha256sumsUrl -OutFile $sha256sumsPath

        $assetPattern = "(?im)^([a-f0-9]{64})\s+\*?$([regex]::Escape($AssetName))\s*$"
        $match = [regex]::Match((Get-Content -LiteralPath $sha256sumsPath -Raw), $assetPattern)
        if (-not $match.Success) {
            throw "Could not find checksum entry for $AssetName in SHA256SUMS."
        }
        return $match.Groups[1].Value
    }
}

function Verify-ChecksumFile {
    param(
        [string]$FilePath,
        [string]$ExpectedChecksum
    )
    $expected = Parse-ChecksumHex -ChecksumText $ExpectedChecksum
    $actual = Get-Sha256Hex -FilePath $FilePath
    if ($actual -ne $expected) {
        throw "Checksum verification failed. Expected $expected but got $actual. Re-run installer to fetch fresh artifacts; if using a manual checksum, verify it matches the release asset."
    }
    Write-Ok "Checksum verified ($($actual.Substring(0, 16))...)"
}

function Install-BinariesAtomically {
    param(
        [string]$AmSource,
        [string]$ServerSource,
        [string]$InstallDir
    )

    if (-not (Test-Path -LiteralPath $AmSource)) {
        throw "Atomic install source missing: $AmSource. Release archive may be incomplete; retry download or pin a known-good -Version."
    }
    if (-not (Test-Path -LiteralPath $ServerSource)) {
        throw "Atomic install source missing: $ServerSource. Release archive may be incomplete; retry download or pin a known-good -Version."
    }

    New-Item -ItemType Directory -Path $InstallDir -Force | Out-Null

    $amDest = Join-Path $InstallDir "am.exe"
    $serverDest = Join-Path $InstallDir "mcp-agent-mail.exe"
    $nonce = [Guid]::NewGuid().ToString("N")
    $stamp = Get-Date -Format "yyyyMMdd_HHmmss"

    $amTemp = "$amDest.tmp.$nonce"
    $serverTemp = "$serverDest.tmp.$nonce"
    $amBackup = $null
    $serverBackup = $null

    try {
        Copy-Item -LiteralPath $AmSource -Destination $amTemp -Force
        Copy-Item -LiteralPath $ServerSource -Destination $serverTemp -Force

        if (Test-Path -LiteralPath $amDest) {
            $amBackup = "$amDest.bak.preinstall-$stamp-$nonce"
            Move-Item -LiteralPath $amDest -Destination $amBackup -Force
        }
        if (Test-Path -LiteralPath $serverDest) {
            $serverBackup = "$serverDest.bak.preinstall-$stamp-$nonce"
            Move-Item -LiteralPath $serverDest -Destination $serverBackup -Force
        }

        Move-Item -LiteralPath $amTemp -Destination $amDest -Force
        Move-Item -LiteralPath $serverTemp -Destination $serverDest -Force

        if ($null -ne $amBackup -and (Test-Path -LiteralPath $amBackup)) {
            Remove-Item -LiteralPath $amBackup -Force -ErrorAction SilentlyContinue
        }
        if ($null -ne $serverBackup -and (Test-Path -LiteralPath $serverBackup)) {
            Remove-Item -LiteralPath $serverBackup -Force -ErrorAction SilentlyContinue
        }
    } catch {
        $installError = $_.Exception.Message
        if (Test-Path -LiteralPath $amDest) {
            Remove-Item -LiteralPath $amDest -Force -ErrorAction SilentlyContinue
        }
        if (Test-Path -LiteralPath $serverDest) {
            Remove-Item -LiteralPath $serverDest -Force -ErrorAction SilentlyContinue
        }
        if ($null -ne $amBackup -and (Test-Path -LiteralPath $amBackup)) {
            Move-Item -LiteralPath $amBackup -Destination $amDest -Force -ErrorAction SilentlyContinue
        }
        if ($null -ne $serverBackup -and (Test-Path -LiteralPath $serverBackup)) {
            Move-Item -LiteralPath $serverBackup -Destination $serverDest -Force -ErrorAction SilentlyContinue
        }
        throw "Atomic binary replacement failed. Rollback attempted. Close any running am/mcp-agent-mail processes and re-run with -Force. Root error: $installError"
    } finally {
        if (Test-Path -LiteralPath $amTemp) {
            Remove-Item -LiteralPath $amTemp -Force -ErrorAction SilentlyContinue
        }
        if (Test-Path -LiteralPath $serverTemp) {
            Remove-Item -LiteralPath $serverTemp -Force -ErrorAction SilentlyContinue
        }
    }
}

function Get-PythonProbeSpecs {
    return @(
        @{ Exe = "py"; Args = @("-3") },
        @{ Exe = "python"; Args = @() },
        @{ Exe = "python3"; Args = @() }
    )
}

function Test-PythonModuleAvailable {
    $moduleScript = "import importlib.util,sys;sys.exit(0 if importlib.util.find_spec('mcp_agent_mail') else 1)"
    foreach ($probe in (Get-PythonProbeSpecs)) {
        $exe = [string]$probe.Exe
        if (-not (Get-Command $exe -ErrorAction SilentlyContinue)) {
            continue
        }
        try {
            & $exe @($probe.Args + @("-c", $moduleScript)) *> $null
            if ($LASTEXITCODE -eq 0) {
                return $true
            }
        } catch {
            continue
        }
    }
    return $false
}

function Get-PythonScriptDirCandidates {
    $dirs = @()

    foreach ($probe in (Get-PythonProbeSpecs)) {
        $exe = [string]$probe.Exe
        if (-not (Get-Command $exe -ErrorAction SilentlyContinue)) {
            continue
        }
        try {
            $scriptDir = (& $exe @($probe.Args + @("-c", "import sysconfig; print(sysconfig.get_path('scripts') or '')")) 2>$null | Select-Object -First 1)
            if (-not [string]::IsNullOrWhiteSpace($scriptDir)) {
                $dirs += ([string]$scriptDir).Trim()
            }
        } catch {
            continue
        }
    }

    $commonDirs = @(
        (Join-Path $HOME "mcp_agent_mail\.venv\Scripts"),
        (Join-Path $HOME "mcp_agent_mail\venv\Scripts"),
        (Join-Path $HOME "mcp-agent-mail\.venv\Scripts"),
        (Join-Path $HOME "mcp-agent-mail\venv\Scripts")
    )
    foreach ($base in $commonDirs) {
        if (-not (Test-Path -LiteralPath $base)) {
            continue
        }
        if ((Get-Item -LiteralPath $base).PSIsContainer) {
            $dirs += $base
            $dirs += (Get-ChildItem -LiteralPath $base -Directory -ErrorAction SilentlyContinue | ForEach-Object { $_.FullName })
        }
    }

    $globPatterns = @(
        (Join-Path $env:APPDATA "Python\Python*\Scripts"),
        (Join-Path $env:LOCALAPPDATA "Programs\Python\Python*\Scripts")
    )
    foreach ($pattern in $globPatterns) {
        try {
            $dirs += (Get-ChildItem -Path $pattern -Directory -ErrorAction SilentlyContinue | ForEach-Object { $_.FullName })
        } catch {
            continue
        }
    }

    $resolved = @()
    $seen = @{}
    foreach ($dir in $dirs) {
        if ([string]::IsNullOrWhiteSpace($dir)) {
            continue
        }
        $norm = $dir.TrimEnd("\").ToLowerInvariant()
        if ($seen.ContainsKey($norm)) {
            continue
        }
        $seen[$norm] = $true
        $resolved += $dir
    }
    return $resolved
}

function Get-PythonAmExecutables {
    param([string]$InstallDir)

    $paths = @()
    foreach ($dir in (Get-PythonScriptDirCandidates)) {
        $candidate = Join-Path $dir "am.exe"
        if (Test-Path -LiteralPath $candidate) {
            $paths += $candidate
        }
    }

    $cmdHits = Get-Command am -All -ErrorAction SilentlyContinue
    foreach ($hit in $cmdHits) {
        if ($null -eq $hit.Source) {
            continue
        }
        if ($hit.Source -match 'am\.exe$') {
            $paths += $hit.Source
        }
    }

    $seen = @{}
    $normalizedInstallDir = $InstallDir.TrimEnd("\").ToLowerInvariant()
    $result = @()
    foreach ($path in $paths) {
        if ([string]::IsNullOrWhiteSpace($path)) {
            continue
        }
        $fullPath = [System.IO.Path]::GetFullPath($path)
        if (-not (Test-Path -LiteralPath $fullPath)) {
            continue
        }
        $norm = $fullPath.ToLowerInvariant()
        if ($seen.ContainsKey($norm)) {
            continue
        }
        $seen[$norm] = $true
        if ($norm.StartsWith($normalizedInstallDir + "\")) {
            continue
        }
        if ($norm -match '\\scripts\\am\.exe$' -or $norm -match '\\\.venv\\scripts\\am\.exe$' -or $norm -match '\\venv\\scripts\\am\.exe$') {
            $result += $fullPath
        }
    }

    return $result
}

function Displace-PythonAmExecutables {
    param([string[]]$Paths)
    $moved = @()
    foreach ($path in $Paths) {
        if (-not (Test-Path -LiteralPath $path)) {
            continue
        }
        $parent = Split-Path -LiteralPath $path -Parent
        $stamp = Get-Date -Format "yyyyMMdd_HHmmss"
        $backupName = "am.exe.bak.mcp-agent-mail-$stamp"
        $backupPath = Join-Path $parent $backupName
        $suffix = 1
        while (Test-Path -LiteralPath $backupPath) {
            $backupPath = Join-Path $parent ("am.exe.bak.mcp-agent-mail-$stamp-$suffix")
            $suffix++
        }

        try {
            Move-Item -LiteralPath $path -Destination $backupPath -Force
            $moved += "$path -> $backupPath"
        } catch {
            Write-WarnText "Failed to displace Python am.exe at $path ($($_.Exception.Message))"
        }
    }
    return $moved
}

function Ensure-SqliteDll {
    param(
        [string]$ExtractDir,
        [string]$InstallDir,
        [string]$ResolvedVersion
    )
    Write-Verbose "Ensure-SqliteDll: no-op; current Windows binaries do not require sqlite3.dll."
}

function Verify-Install {
    param([string]$InstallDir)
    $amExe = Join-Path $InstallDir "am.exe"
    $serverExe = Join-Path $InstallDir "mcp-agent-mail.exe"

    if (-not (Test-Path -LiteralPath $amExe)) {
        throw "Install verification failed: $amExe is missing. Re-run with -Force and verify antivirus did not quarantine files under $InstallDir."
    }
    if (-not (Test-Path -LiteralPath $serverExe)) {
        throw "Install verification failed: $serverExe is missing. Re-run with -Force and verify antivirus did not quarantine files under $InstallDir."
    }

    $amVersion = (& $amExe --version 2>$null | Select-Object -First 1)
    $serverVersion = (& $serverExe --version 2>$null | Select-Object -First 1)

    if ([string]::IsNullOrWhiteSpace($amVersion)) {
        throw "Install verification failed: am.exe --version returned no output. Re-run with -Force and run '$amExe --version' manually for diagnostics."
    }
    if ([string]::IsNullOrWhiteSpace($serverVersion)) {
        throw "Install verification failed: mcp-agent-mail.exe --version returned no output. Re-run with -Force and run '$serverExe --version' manually for diagnostics."
    }

    Write-Ok "VERIFY am.exe -> $amVersion"
    Write-Ok "VERIFY mcp-agent-mail.exe -> $serverVersion"
}

$resolvedVersion = Resolve-Version -RequestedVersion $Version
$requestedNormalized = Normalize-Version -RawVersion $resolvedVersion
Write-Info "Installing mcp-agent-mail $resolvedVersion for target $Target"

$installedVersionRaw = Get-InstalledVersion -InstallDir $Dest
if (-not [string]::IsNullOrWhiteSpace($installedVersionRaw) -and -not $Force) {
    $installedNormalized = Normalize-Version -RawVersion $installedVersionRaw
    if ($installedNormalized -eq $requestedNormalized) {
        Write-Ok "mcp-agent-mail $resolvedVersion is already installed at $Dest"
        Write-Host "Use -Force to reinstall."
        exit 0
    }
}

$workDir = Join-Path ([System.IO.Path]::GetTempPath()) ("mcp-agent-mail-install-" + [Guid]::NewGuid().ToString("N"))
New-Item -ItemType Directory -Path $workDir -Force | Out-Null

try {
    $zipPath = Join-Path $workDir $AssetName
    $extractDir = Join-Path $workDir "extract"
    $assetUrl = "https://github.com/$Owner/$Repo/releases/download/$resolvedVersion/$AssetName"
    Write-Info "Downloading $assetUrl"
    Download-File -Url $assetUrl -OutFile $zipPath

    if ($ShouldVerifyChecksum) {
        $checksumText = Resolve-ChecksumText -AssetUrl $assetUrl -AssetName $AssetName -WorkDir $workDir
        Verify-ChecksumFile -FilePath $zipPath -ExpectedChecksum $checksumText
    } else {
        Write-WarnText "Checksum verification skipped (-NoVerify)"
    }

    Write-Info "Extracting archive"
    Expand-Archive -LiteralPath $zipPath -DestinationPath $extractDir -Force

    $amSource = Get-ChildItem -LiteralPath $extractDir -Filter "am.exe" -Recurse | Select-Object -First 1
    $serverSource = Get-ChildItem -LiteralPath $extractDir -Filter "mcp-agent-mail.exe" -Recurse | Select-Object -First 1
    if ($null -eq $amSource -or $null -eq $serverSource) {
        throw "Release archive did not contain am.exe and mcp-agent-mail.exe. Retry download, pin a known-good -Version, or report at $IssuesUrl. Release list: $ReleasesUrl"
    }

    Install-BinariesAtomically -AmSource $amSource.FullName -ServerSource $serverSource.FullName -InstallDir $Dest
    Write-Ok "Installed binaries to $Dest (atomic replace)"

    $pythonModulePresent = Test-PythonModuleAvailable
    $pythonAmExecutables = @(Get-PythonAmExecutables -InstallDir $Dest)
    if ($pythonModulePresent -or $pythonAmExecutables.Count -gt 0) {
        Write-Info "Detected existing Python mcp-agent-mail footprint"
    }
    if ($pythonAmExecutables.Count -gt 0) {
        $displaced = @(Displace-PythonAmExecutables -Paths $pythonAmExecutables)
        foreach ($entry in $displaced) {
            Write-Ok "Displaced Python am.exe: $entry"
        }
    } elseif ($pythonModulePresent) {
        Write-WarnText "python -m mcp_agent_mail is importable, but no Python am.exe script was found to displace."
    }

    Ensure-SqliteDll -ExtractDir $extractDir -InstallDir $Dest -ResolvedVersion $resolvedVersion

    if (Ensure-UserPathEntry -InstallDir $Dest) {
        Write-Ok "Updated user PATH with $Dest at highest precedence"
    } else {
        Write-Info "User PATH already prioritizes $Dest"
    }

    Verify-Install -InstallDir $Dest
} finally {
    if (Test-Path -LiteralPath $workDir) {
        Remove-Item -LiteralPath $workDir -Recurse -Force -ErrorAction SilentlyContinue
    }
}

Write-Host ""
Write-Ok "mcp-agent-mail is installed."
Write-Host "Quick start:"
Write-Host "  am"
Write-Host "  am serve-http"
Write-Host "  mcp-agent-mail"
Write-Host "  am --help"
