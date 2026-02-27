<#
  mcp-agent-mail installer (Windows)

  One-liner:
    iwr -useb "https://raw.githubusercontent.com/Dicklesworthstone/mcp_agent_mail_rust/main/install.ps1?$(Get-Random)" | iex

  Options:
    -Version vX.Y.Z   Install a specific release tag (default: latest)
    -Dest PATH        Install directory (default: %LOCALAPPDATA%\Programs\mcp-agent-mail)
    -Force            Reinstall even if the same version is already present
#>

[CmdletBinding()]
param(
    [string]$Version = "",
    [string]$Dest = "",
    [switch]$Force
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

$Owner = "Dicklesworthstone"
$Repo = "mcp_agent_mail_rust"
$Target = "x86_64-pc-windows-msvc"
$AssetName = "mcp-agent-mail-$Target.zip"
$DefaultDest = Join-Path $env:LOCALAPPDATA "Programs\mcp-agent-mail"

if ([string]::IsNullOrWhiteSpace($Dest)) {
    $Dest = $DefaultDest
}
$Dest = [System.IO.Path]::GetFullPath($Dest)

if ([System.Environment]::OSVersion.Platform -ne [System.PlatformID]::Win32NT) {
    throw "install.ps1 is only supported on Windows."
}

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
    if ($trimmed.StartsWith("v")) {
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
        throw "Unable to resolve latest release tag from $latestUrl"
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
    $sqliteDest = Join-Path $InstallDir "sqlite3.dll"
    if (Test-Path -LiteralPath $sqliteDest) {
        return
    }

    $sqliteInArchive = Get-ChildItem -LiteralPath $ExtractDir -Filter "sqlite3.dll" -Recurse -ErrorAction SilentlyContinue | Select-Object -First 1
    if ($null -ne $sqliteInArchive) {
        Copy-Item -LiteralPath $sqliteInArchive.FullName -Destination $sqliteDest -Force
        Write-Ok "Bundled sqlite3.dll installed"
        return
    }

    $candidateAssets = @(
        "https://github.com/$Owner/$Repo/releases/download/$ResolvedVersion/sqlite3.dll",
        "https://github.com/$Owner/$Repo/releases/download/$ResolvedVersion/mcp-agent-mail-$Target-sqlite3.dll"
    )

    foreach ($assetUrl in $candidateAssets) {
        try {
            Download-File -Url $assetUrl -OutFile $sqliteDest
            Write-Ok "Downloaded sqlite3.dll from release assets"
            return
        } catch {
            if (Test-Path -LiteralPath $sqliteDest) {
                Remove-Item -LiteralPath $sqliteDest -Force -ErrorAction SilentlyContinue
            }
        }
    }

    Write-WarnText "sqlite3.dll was not found in release assets. If startup reports a missing sqlite3.dll, place sqlite3.dll next to am.exe."
}

function Verify-Install {
    param([string]$InstallDir)
    $amExe = Join-Path $InstallDir "am.exe"
    $serverExe = Join-Path $InstallDir "mcp-agent-mail.exe"

    if (-not (Test-Path -LiteralPath $amExe)) {
        throw "Install verification failed: $amExe is missing."
    }
    if (-not (Test-Path -LiteralPath $serverExe)) {
        throw "Install verification failed: $serverExe is missing."
    }

    $amVersion = (& $amExe --version 2>$null | Select-Object -First 1)
    $serverVersion = (& $serverExe --version 2>$null | Select-Object -First 1)

    if ([string]::IsNullOrWhiteSpace($amVersion)) {
        throw "Install verification failed: am.exe --version returned no output."
    }
    if ([string]::IsNullOrWhiteSpace($serverVersion)) {
        throw "Install verification failed: mcp-agent-mail.exe --version returned no output."
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

    Write-Info "Extracting archive"
    Expand-Archive -LiteralPath $zipPath -DestinationPath $extractDir -Force

    $amSource = Get-ChildItem -LiteralPath $extractDir -Filter "am.exe" -Recurse | Select-Object -First 1
    $serverSource = Get-ChildItem -LiteralPath $extractDir -Filter "mcp-agent-mail.exe" -Recurse | Select-Object -First 1
    if ($null -eq $amSource -or $null -eq $serverSource) {
        throw "Release archive did not contain am.exe and mcp-agent-mail.exe."
    }

    New-Item -ItemType Directory -Path $Dest -Force | Out-Null
    Copy-Item -LiteralPath $amSource.FullName -Destination (Join-Path $Dest "am.exe") -Force
    Copy-Item -LiteralPath $serverSource.FullName -Destination (Join-Path $Dest "mcp-agent-mail.exe") -Force
    Write-Ok "Installed binaries to $Dest"

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
