<#
  mcp-agent-mail installer (Windows)

  One-liner:
    iwr -useb "https://raw.githubusercontent.com/Dicklesworthstone/mcp_agent_mail_rust/main/install.ps1?$(Get-Random)" | iex

  Options:
    -Version vX.Y.Z   Install a specific release tag (default: latest)
    -Dest PATH        Install directory (default: %LOCALAPPDATA%\Programs\mcp-agent-mail)
    -Force            Reinstall even if the same version is already present
    -NoVerify         UNSAFE: skip checksum + Sigstore checks (shape/version checks remain)
    -Verify           Explicitly require archive verification (already the default)
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
$CosignIdentity = ""
$CosignOidcIssuer = 'https://token.actions.githubusercontent.com'

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

$ShouldVerifyArchive = if ($NoVerify) { $false } else { $true }

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

function Get-ReleaseContract {
    param([string]$RawVersion)
    if ([string]::IsNullOrWhiteSpace($RawVersion)) {
        throw "Release version is empty. Pass -Version vX.Y.Z or allow the installer to resolve the latest release."
    }
    $trimmed = $RawVersion.Trim()
    $releaseMatch = [regex]::Match(
        $trimmed,
        '^v?(?<version>[0-9]+\.[0-9]+\.[0-9]+(?:-[0-9A-Za-z][0-9A-Za-z.-]*)?)$'
    )
    if (-not $releaseMatch.Success) {
        throw "Invalid release version '$trimmed'. Expected vX.Y.Z or vX.Y.Z-prerelease (a leading v is optional)."
    }

    $normalizedVersion = $releaseMatch.Groups['version'].Value
    $normalizedTag = "v$normalizedVersion"
    return [pscustomobject]@{
        Tag = $normalizedTag
        Version = $normalizedVersion
        CertificateIdentity = "https://github.com/Dicklesworthstone/mcp_agent_mail_rust/.github/workflows/dist.yml@refs/tags/$normalizedTag"
    }
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
    if ($null -eq (Get-Command Get-FileHash -ErrorAction SilentlyContinue)) {
        throw "No SHA256 implementation is available (Get-FileHash was not found). Install a PowerShell version with Get-FileHash, or use -NoVerify only for a trusted local artifact."
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

function Verify-SigstoreBundle {
    param(
        [string]$FilePath,
        [string]$AssetUrl,
        [string]$WorkDir
    )

    $cosignCommand = Get-Command cosign -CommandType Application -ErrorAction SilentlyContinue | Select-Object -First 1
    if ($null -eq $cosignCommand) {
        throw "cosign is required to verify release archive authenticity but was not found. Install cosign, or use -NoVerify only for a trusted local artifact."
    }

    $bundleUrl = "$AssetUrl.sigstore.json"
    $bundlePath = Join-Path $WorkDir "release.sigstore.json"
    Write-Info "Downloading Sigstore bundle $bundleUrl"
    try {
        Download-File -Url $bundleUrl -OutFile $bundlePath
    } catch {
        throw "Sigstore bundle download failed at $bundleUrl. Release archives are not extracted without a signature unless -NoVerify is explicit. Root error: $($_.Exception.Message)"
    }

    if (-not (Test-Path -LiteralPath $bundlePath -PathType Leaf)) {
        throw "Sigstore bundle is missing after download: $bundlePath"
    }
    $bundleText = Get-Content -LiteralPath $bundlePath -Raw
    if ([string]::IsNullOrWhiteSpace($bundleText)) {
        throw "Sigstore bundle is empty: $bundlePath"
    }
    try {
        $null = $bundleText | ConvertFrom-Json -ErrorAction Stop
    } catch {
        throw "Sigstore bundle is malformed JSON: $bundlePath. Root error: $($_.Exception.Message)"
    }

    $cosignArgs = @(
        "verify-blob",
        "--bundle", $bundlePath,
        "--certificate-identity", $CosignIdentity,
        "--certificate-oidc-issuer", $CosignOidcIssuer,
        $FilePath
    )
    $cosignOutput = @(& $cosignCommand.Source @cosignArgs 2>&1)
    if ($LASTEXITCODE -ne 0) {
        $detail = ($cosignOutput | ForEach-Object { [string]$_ }) -join [Environment]::NewLine
        throw "Sigstore verification failed. The bundle must be valid and signed by $CosignIdentity via $CosignOidcIssuer. cosign output: $detail"
    }

    Write-Ok "Signature verified (cosign)"
}

function Assert-ExactArchiveMembers {
    param([string]$ArchivePath)

    if (-not (Test-Path -LiteralPath $ArchivePath -PathType Leaf)) {
        throw "Release archive is missing: $ArchivePath"
    }

    Add-Type -AssemblyName System.IO.Compression.FileSystem
    $archive = [System.IO.Compression.ZipFile]::OpenRead($ArchivePath)
    try {
        $entries = @($archive.Entries)
        $names = @($entries | ForEach-Object { $_.FullName })
        $namesAreExact = (
            $entries.Count -eq 2 -and
            $names -ccontains "am.exe" -and
            $names -ccontains "mcp-agent-mail.exe"
        )
        if (-not $namesAreExact) {
            $observed = if ($names.Count -eq 0) { "<empty>" } else { $names -join ", " }
            throw "Release archive members are '$observed'; expected exactly flat am.exe and mcp-agent-mail.exe."
        }

        foreach ($entry in $entries) {
            $unixMode = ($entry.ExternalAttributes -shr 16) -band 0xFFFF
            $fileType = $unixMode -band 0xF000
            if ($entry.Length -le 0 -or ($fileType -ne 0 -and $fileType -ne 0x8000)) {
                throw "Release archive member '$($entry.FullName)' is empty or is not a regular file."
            }
        }
    } finally {
        $archive.Dispose()
    }
}

function Assert-ExactBinaryVersion {
    param(
        [string]$BinaryPath,
        [string]$ExpectedOutput,
        [string]$Phase
    )

    if (-not (Test-Path -LiteralPath $BinaryPath -PathType Leaf)) {
        throw "$Phase binary is missing: $BinaryPath"
    }

    try {
        $versionLines = @(& $BinaryPath --version 2>&1)
        $versionExit = $LASTEXITCODE
    } catch {
        throw "$Phase version probe could not execute '$BinaryPath': $($_.Exception.Message)"
    }

    $actual = ($versionLines | ForEach-Object { [string]$_ }) -join [Environment]::NewLine
    if ($versionExit -ne 0 -or $versionLines.Count -ne 1 -or $actual -cne $ExpectedOutput) {
        $displayActual = if ([string]::IsNullOrEmpty($actual)) { "<no version output>" } else { $actual }
        throw "$Phase '$BinaryPath --version' reported '$displayActual' (exit $versionExit); expected exactly '$ExpectedOutput'."
    }
}

function Test-InstalledReleaseVersion {
    param(
        [string]$InstallDir,
        [string]$ExpectedVersion
    )

    try {
        Assert-ExactBinaryVersion `
            -BinaryPath (Join-Path $InstallDir "am.exe") `
            -ExpectedOutput "am $ExpectedVersion" `
            -Phase "Installed"
        Assert-ExactBinaryVersion `
            -BinaryPath (Join-Path $InstallDir "mcp-agent-mail.exe") `
            -ExpectedOutput "mcp-agent-mail $ExpectedVersion" `
            -Phase "Installed"
        return $true
    } catch {
        return $false
    }
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
    param(
        [string]$InstallDir,
        [string]$ExpectedVersion
    )
    $amExe = Join-Path $InstallDir "am.exe"
    $serverExe = Join-Path $InstallDir "mcp-agent-mail.exe"

    if (-not (Test-Path -LiteralPath $amExe)) {
        throw "Install verification failed: $amExe is missing. Re-run with -Force and verify antivirus did not quarantine files under $InstallDir."
    }
    if (-not (Test-Path -LiteralPath $serverExe)) {
        throw "Install verification failed: $serverExe is missing. Re-run with -Force and verify antivirus did not quarantine files under $InstallDir."
    }

    Assert-ExactBinaryVersion -BinaryPath $amExe -ExpectedOutput "am $ExpectedVersion" -Phase "Post-install"
    Assert-ExactBinaryVersion -BinaryPath $serverExe -ExpectedOutput "mcp-agent-mail $ExpectedVersion" -Phase "Post-install"
    Write-Ok "VERIFY am.exe -> am $ExpectedVersion"
    Write-Ok "VERIFY mcp-agent-mail.exe -> mcp-agent-mail $ExpectedVersion"
}

$requestedRelease = Resolve-Version -RequestedVersion $Version
$releaseContract = Get-ReleaseContract -RawVersion $requestedRelease
$resolvedVersion = $releaseContract.Tag
$requestedNormalized = $releaseContract.Version
$CosignIdentity = $releaseContract.CertificateIdentity
Write-Info "Installing mcp-agent-mail $resolvedVersion for target $Target"

if (-not $Force -and (Test-InstalledReleaseVersion -InstallDir $Dest -ExpectedVersion $requestedNormalized)) {
    Write-Ok "mcp-agent-mail $resolvedVersion is already installed at $Dest"
    Write-Host "Use -Force to reinstall."
    exit 0
}

$workDir = Join-Path ([System.IO.Path]::GetTempPath()) ("mcp-agent-mail-install-" + [Guid]::NewGuid().ToString("N"))
New-Item -ItemType Directory -Path $workDir -Force | Out-Null

try {
    $zipPath = Join-Path $workDir $AssetName
    $extractDir = Join-Path $workDir "extract"
    $assetUrl = "https://github.com/$Owner/$Repo/releases/download/$resolvedVersion/$AssetName"
    Write-Info "Downloading $assetUrl"
    Download-File -Url $assetUrl -OutFile $zipPath

    if ($ShouldVerifyArchive) {
        $checksumText = Resolve-ChecksumText -AssetUrl $assetUrl -AssetName $AssetName -WorkDir $workDir
        Verify-ChecksumFile -FilePath $zipPath -ExpectedChecksum $checksumText
        Verify-SigstoreBundle -FilePath $zipPath -AssetUrl $assetUrl -WorkDir $workDir
    } else {
        Write-WarnText "UNSAFE: archive checksum and Sigstore verification skipped (-NoVerify)"
        Write-WarnText "Archive-member and exact-version checks remain mandatory."
    }

    Assert-ExactArchiveMembers -ArchivePath $zipPath
    Write-Info "Extracting archive"
    Expand-Archive -LiteralPath $zipPath -DestinationPath $extractDir -Force

    $amSource = Join-Path $extractDir "am.exe"
    $serverSource = Join-Path $extractDir "mcp-agent-mail.exe"
    foreach ($stagedPath in @($amSource, $serverSource)) {
        if (-not (Test-Path -LiteralPath $stagedPath -PathType Leaf)) {
            throw "Release archive did not extract the expected regular file '$stagedPath'. Retry download, pin a known-good -Version, or report at $IssuesUrl. Release list: $ReleasesUrl"
        }
        $stagedItem = Get-Item -LiteralPath $stagedPath
        if (($stagedItem.Attributes -band [System.IO.FileAttributes]::ReparsePoint) -ne 0 -or $stagedItem.Length -le 0) {
            throw "Release archive member '$stagedPath' is empty or is a reparse point; refusing installation."
        }
    }

    Assert-ExactBinaryVersion -BinaryPath $amSource -ExpectedOutput "am $requestedNormalized" -Phase "Staged"
    Assert-ExactBinaryVersion -BinaryPath $serverSource -ExpectedOutput "mcp-agent-mail $requestedNormalized" -Phase "Staged"
    Write-Ok "Staged binaries match release $resolvedVersion"

    Install-BinariesAtomically -AmSource $amSource -ServerSource $serverSource -InstallDir $Dest
    Write-Ok "Installed binaries to $Dest (atomic replace)"
    Verify-Install -InstallDir $Dest -ExpectedVersion $requestedNormalized

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
