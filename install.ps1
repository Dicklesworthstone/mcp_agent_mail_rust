<#
  mcp-agent-mail installer (Windows)

  One-liner:
    iwr -useb "https://raw.githubusercontent.com/Dicklesworthstone/mcp_agent_mail_rust/main/install.ps1?$(Get-Random)" | iex

  Options:
    -Version vX.Y.Z   Install a specific release tag (default: latest)
    -Dest PATH        Install directory (default: %LOCALAPPDATA%\Programs\mcp-agent-mail)
    -Force            Reinstall without probing the already-installed version
    -NoVerify         UNSAFE: skip checksum + Sigstore checks; downloaded code still executes
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

function Stop-VersionProbeProcessTree {
    param([System.Diagnostics.Process]$Process)

    if ($null -eq $Process) {
        return
    }
    try {
        if ([System.Environment]::OSVersion.Platform -eq [System.PlatformID]::Win32NT) {
            $taskkill = Join-Path $env:SystemRoot "System32\taskkill.exe"
            if (Test-Path -LiteralPath $taskkill -PathType Leaf) {
                & $taskkill /PID $Process.Id /T /F *> $null
            } else {
                $Process.Kill()
            }
        } else {
            try { $Process.Kill($true) } catch { $Process.Kill() }
        }
    } catch {
        try { $Process.Kill() } catch { }
    }
    try { $null = $Process.WaitForExit(1000) } catch { }
}

function Invoke-VersionProbeBounded {
    param(
        [string]$BinaryPath,
        [ValidateSet("--version", "version")]
        [string]$Argument,
        [int]$TimeoutMilliseconds = 3000
    )

    $startInfo = [System.Diagnostics.ProcessStartInfo]::new()
    $startInfo.FileName = $BinaryPath
    $startInfo.Arguments = $Argument
    $startInfo.UseShellExecute = $false
    $startInfo.CreateNoWindow = $true
    $startInfo.RedirectStandardOutput = $true
    $startInfo.RedirectStandardError = $true
    $process = [System.Diagnostics.Process]::new()
    $process.StartInfo = $startInfo
    $stopwatch = [System.Diagnostics.Stopwatch]::StartNew()

    try {
        if (-not $process.Start()) {
            throw "process start returned false"
        }
        $stdoutTask = $process.StandardOutput.ReadToEndAsync()
        $stderrTask = $process.StandardError.ReadToEndAsync()
        [int]$remaining = [Math]::Max(0, $TimeoutMilliseconds - [int]$stopwatch.ElapsedMilliseconds)
        if ($remaining -eq 0 -or -not $process.WaitForExit($remaining)) {
            Stop-VersionProbeProcessTree -Process $process
            throw "version probe timed out after $TimeoutMilliseconds ms"
        }

        # A process can exit after spawning a descendant that retains its
        # redirected stdout/stderr handles. Bound the stream drain as part of
        # the same deadline so GetResult() can never wait forever on that child.
        $remaining = [Math]::Max(0, $TimeoutMilliseconds - [int]$stopwatch.ElapsedMilliseconds)
        [System.Threading.Tasks.Task[]]$ioTasks = @($stdoutTask, $stderrTask)
        if ($remaining -eq 0 -or
            -not [System.Threading.Tasks.Task]::WaitAll($ioTasks, $remaining)) {
            Stop-VersionProbeProcessTree -Process $process
            throw "version probe output did not close within $TimeoutMilliseconds ms"
        }

        return [pscustomobject]@{
            ExitCode = $process.ExitCode
            Stdout = $stdoutTask.GetAwaiter().GetResult()
            Stderr = $stderrTask.GetAwaiter().GetResult()
        }
    } finally {
        $process.Dispose()
    }
}

function Assert-SafeCosignVersion {
    param([string[]]$VersionOutput)

    $versions = @()
    foreach ($line in $VersionOutput) {
        $match = [regex]::Match(
            [string]$line,
            '^\s*GitVersion:\s*v?(?<major>[0-9]+)\.(?<minor>[0-9]+)\.(?<patch>[0-9]+)\s*$'
        )
        if ($match.Success) {
            $versions += [version]::new(
                [int]$match.Groups['major'].Value,
                [int]$match.Groups['minor'].Value,
                [int]$match.Groups['patch'].Value
            )
        }
    }
    if ($versions.Count -ne 1) {
        throw "Could not parse exactly one stable GitVersion from cosign output. Release verification requires cosign >=3.1.3 and <4.0.0."
    }
    $version = $versions[0]
    if ($version.Major -ne 3 -or $version -lt [version]"3.1.3") {
        throw "Unsafe or unsupported cosign version v$version; require >=v3.1.3 and <v4.0.0."
    }
    return $version
}

function Get-SafeCosignPath {
    $cosignCommand = Get-Command cosign -CommandType Application -ErrorAction SilentlyContinue | Select-Object -First 1
    if ($null -eq $cosignCommand) {
        throw "cosign is required to verify release archive authenticity but was not found. Install cosign >=v3.1.3 and <v4.0.0, or use -NoVerify only for a trusted local artifact."
    }
    try {
        $probe = Invoke-VersionProbeBounded -BinaryPath $cosignCommand.Source -Argument "version"
    } catch {
        throw "Could not determine a bounded cosign version: $($_.Exception.Message)"
    }
    if ($probe.ExitCode -ne 0 -or -not [string]::IsNullOrEmpty($probe.Stderr)) {
        throw "cosign version failed or wrote diagnostics; require a stable cosign >=v3.1.3 and <v4.0.0."
    }
    $versionLines = @($probe.Stdout -split "\r?\n" | Where-Object { $_ -ne "" })
    $safeVersion = Assert-SafeCosignVersion -VersionOutput $versionLines
    Write-Verbose "Using cosign v$safeVersion at $($cosignCommand.Source)"
    return $cosignCommand.Source
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

    $cosignPath = Get-SafeCosignPath

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
        "--new-bundle-format",
        "--bundle", $bundlePath,
        "--certificate-identity", $CosignIdentity,
        "--certificate-oidc-issuer", $CosignOidcIssuer,
        $FilePath
    )
    $trustEnvironmentNames = @(
        "SIGSTORE_ROOT_FILE",
        "SIGSTORE_REKOR_PUBLIC_KEY",
        "SIGSTORE_CT_LOG_PUBLIC_KEY_FILE"
    )
    $savedTrustEnvironment = @{}
    foreach ($name in $trustEnvironmentNames) {
        $savedTrustEnvironment[$name] = [Environment]::GetEnvironmentVariable($name, "Process")
        [Environment]::SetEnvironmentVariable($name, $null, "Process")
    }
    try {
        $cosignOutput = @(& $cosignPath @cosignArgs 2>&1)
        $cosignExitCode = $LASTEXITCODE
    } finally {
        foreach ($name in $trustEnvironmentNames) {
            [Environment]::SetEnvironmentVariable($name, $savedTrustEnvironment[$name], "Process")
        }
    }
    if ($cosignExitCode -ne 0) {
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
        $probe = Invoke-VersionProbeBounded -BinaryPath $BinaryPath -Argument "--version"
    } catch {
        throw "$Phase version probe could not execute '$BinaryPath': $($_.Exception.Message)"
    }

    $actual = [string]$probe.Stdout
    if ($actual.EndsWith("`r`n", [StringComparison]::Ordinal)) {
        $actual = $actual.Substring(0, $actual.Length - 2)
    } elseif ($actual.EndsWith("`n", [StringComparison]::Ordinal)) {
        $actual = $actual.Substring(0, $actual.Length - 1)
    }
    $hasExtraLines = $actual.Contains("`r") -or $actual.Contains("`n")
    if ($probe.ExitCode -ne 0 -or -not [string]::IsNullOrEmpty($probe.Stderr) -or
        $hasExtraLines -or $actual -cne $ExpectedOutput) {
        $displayActual = if ([string]::IsNullOrEmpty($actual)) { "<no version output>" } else { $actual }
        if (-not [string]::IsNullOrEmpty($probe.Stderr)) {
            $displayActual += " [stderr: $($probe.Stderr)]"
        }
        throw "$Phase '$BinaryPath --version' reported '$displayActual' (exit $($probe.ExitCode)); expected exactly '$ExpectedOutput'."
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

function Assert-SafeInstallDirectory {
    param([string]$InstallDir)

    $fullPath = [System.IO.Path]::GetFullPath($InstallDir)
    $root = [System.IO.Path]::GetPathRoot($fullPath)
    if ([string]::IsNullOrWhiteSpace($root)) {
        throw "Install directory has no filesystem root: $InstallDir"
    }
    if ($fullPath.Length -gt $root.Length) {
        $fullPath = $fullPath.TrimEnd([char[]]@('\', '/'))
    }
    $current = $root
    $relative = $fullPath.Substring($root.Length)
    foreach ($segment in ($relative -split '[\\/]')) {
        if ([string]::IsNullOrWhiteSpace($segment)) {
            continue
        }
        $current = Join-Path $current $segment
        if (Test-Path -LiteralPath $current) {
            $item = Get-Item -LiteralPath $current -Force
            if (-not $item.PSIsContainer -or
                ($item.Attributes -band [System.IO.FileAttributes]::ReparsePoint) -ne 0) {
                throw "Install directory component is not a real directory: $current"
            }
        } else {
            $null = New-Item -ItemType Directory -Path $current
        }
    }
    return $fullPath
}

function Enter-InstallerMutex {
    param(
        [string]$InstallDir,
        [int]$TimeoutMilliseconds = 30000
    )

    $normalizedPath = [System.IO.Path]::GetFullPath($InstallDir)
    $pathRoot = [System.IO.Path]::GetPathRoot($normalizedPath)
    if ($normalizedPath.Length -gt $pathRoot.Length) {
        $normalizedPath = $normalizedPath.TrimEnd([char[]]@('\', '/'))
    }

    $sha = [System.Security.Cryptography.SHA256]::Create()
    try {
        $bytes = [Text.Encoding]::UTF8.GetBytes($normalizedPath.ToLowerInvariant())
        $digest = ($sha.ComputeHash($bytes) | ForEach-Object { $_.ToString("x2") }) -join ""
    } finally {
        $sha.Dispose()
    }
    # The Global namespace coordinates local-console and RDP sessions that can
    # write the same per-user destination. The path digest keeps unrelated
    # destinations independent.
    $mutex = [Threading.Mutex]::new($false, "Global\mcp-agent-mail-install-$digest")
    $acquired = $false
    try {
        try {
            $acquired = $mutex.WaitOne($TimeoutMilliseconds)
        } catch [Threading.AbandonedMutexException] {
            $acquired = $true
        }
        if (-not $acquired) {
            throw "Another installer is operating on $InstallDir. Wait for it to finish and retry."
        }
        return $mutex
    } catch {
        $mutex.Dispose()
        throw
    }
}

function Exit-InstallerMutex {
    param([Threading.Mutex]$Mutex)
    if ($null -eq $Mutex) {
        return
    }
    try { $Mutex.ReleaseMutex() } catch { }
    $Mutex.Dispose()
}

$script:ActiveBinaryTransactionInstallDir = $null
$script:BinaryTransactionRecoveryActive = $false

function Initialize-InstallerNativeMethods {
    if ($null -ne ("McpAgentMailInstallerNativeMethods" -as [type])) {
        return
    }
    Add-Type -TypeDefinition @"
using System;
using System.Runtime.InteropServices;
using Microsoft.Win32.SafeHandles;

public static class McpAgentMailInstallerNativeMethods
{
    [StructLayout(LayoutKind.Sequential)]
    public struct BY_HANDLE_FILE_INFORMATION
    {
        public uint FileAttributes;
        public System.Runtime.InteropServices.ComTypes.FILETIME CreationTime;
        public System.Runtime.InteropServices.ComTypes.FILETIME LastAccessTime;
        public System.Runtime.InteropServices.ComTypes.FILETIME LastWriteTime;
        public uint VolumeSerialNumber;
        public uint FileSizeHigh;
        public uint FileSizeLow;
        public uint NumberOfLinks;
        public uint FileIndexHigh;
        public uint FileIndexLow;
    }

    [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    public static extern SafeFileHandle CreateFileW(
        string fileName,
        uint desiredAccess,
        uint shareMode,
        IntPtr securityAttributes,
        uint creationDisposition,
        uint flagsAndAttributes,
        IntPtr templateFile);

    [DllImport("kernel32.dll", SetLastError = true)]
    [return: MarshalAs(UnmanagedType.Bool)]
    public static extern bool FlushFileBuffers(SafeFileHandle handle);

    [DllImport("kernel32.dll", SetLastError = true)]
    [return: MarshalAs(UnmanagedType.Bool)]
    public static extern bool GetFileInformationByHandle(
        SafeFileHandle handle,
        out BY_HANDLE_FILE_INFORMATION information);

    [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    [return: MarshalAs(UnmanagedType.Bool)]
    public static extern bool MoveFileExW(string existingPath, string newPath, uint flags);
}
"@
}

function Test-InstallerEntryExists {
    param([string]$Path)
    try {
        $null = [System.IO.File]::GetAttributes($Path)
        return $true
    } catch [System.IO.FileNotFoundException] {
        return $false
    } catch [System.IO.DirectoryNotFoundException] {
        return $false
    }
}

function Get-InstallerLinkCount {
    param([string]$Path)
    if ([System.Environment]::OSVersion.Platform -ne [System.PlatformID]::Win32NT) {
        return 1
    }
    Initialize-InstallerNativeMethods
    $shareAll = [uint32]0x00000007
    $openExisting = [uint32]3
    $openReparsePoint = [uint32]0x00200000
    $backupSemantics = [uint32]0x02000000
    $handle = [McpAgentMailInstallerNativeMethods]::CreateFileW(
        $Path, 0, $shareAll, [IntPtr]::Zero, $openExisting,
        ($openReparsePoint -bor $backupSemantics), [IntPtr]::Zero
    )
    if ($handle.IsInvalid) {
        $errorCode = [Runtime.InteropServices.Marshal]::GetLastWin32Error()
        $handle.Dispose()
        throw [ComponentModel.Win32Exception]::new($errorCode, "Could not inspect link count for $Path")
    }
    try {
        $information = [McpAgentMailInstallerNativeMethods+BY_HANDLE_FILE_INFORMATION]::new()
        if (-not [McpAgentMailInstallerNativeMethods]::GetFileInformationByHandle($handle, [ref]$information)) {
            $errorCode = [Runtime.InteropServices.Marshal]::GetLastWin32Error()
            throw [ComponentModel.Win32Exception]::new($errorCode, "Could not inspect link count for $Path")
        }
        return [uint32]$information.NumberOfLinks
    } finally {
        $handle.Dispose()
    }
}

function Assert-InstallerOwnedEntry {
    param(
        [string]$Path,
        [string]$Label,
        [switch]$Directory
    )
    if (-not (Test-InstallerEntryExists -Path $Path)) {
        throw "$Label is missing: $Path"
    }
    $attributes = [System.IO.File]::GetAttributes($Path)
    if (($attributes -band [System.IO.FileAttributes]::ReparsePoint) -ne 0) {
        throw "$Label is a reparse point: $Path"
    }
    $isDirectory = ($attributes -band [System.IO.FileAttributes]::Directory) -ne 0
    if ($Directory -ne $isDirectory) {
        throw "$Label has the wrong filesystem type: $Path"
    }
    if ((Get-InstallerLinkCount -Path $Path) -ne 1) {
        throw "$Label has an unsafe hard-link count: $Path"
    }
    if ([System.Environment]::OSVersion.Platform -eq [System.PlatformID]::Win32NT) {
        $owner = (Get-Acl -LiteralPath $Path).GetOwner([Security.Principal.SecurityIdentifier]).Value
        $current = [Security.Principal.WindowsIdentity]::GetCurrent().User.Value
        if ($owner -cne $current) {
            throw "$Label is owned by $owner instead of the current user: $Path"
        }
    }
}

function Flush-InstallerDirectory {
    param([string]$Path)
    if ([System.Environment]::OSVersion.Platform -ne [System.PlatformID]::Win32NT) {
        return
    }
    Initialize-InstallerNativeMethods
    $genericWrite = [uint32]0x40000000
    $shareAll = [uint32]0x00000007
    $openExisting = [uint32]3
    $backupSemantics = [uint32]0x02000000
    $handle = [McpAgentMailInstallerNativeMethods]::CreateFileW(
        $Path, $genericWrite, $shareAll, [IntPtr]::Zero, $openExisting,
        $backupSemantics, [IntPtr]::Zero
    )
    if ($handle.IsInvalid) {
        $errorCode = [Runtime.InteropServices.Marshal]::GetLastWin32Error()
        $handle.Dispose()
        throw [ComponentModel.Win32Exception]::new($errorCode, "Could not open directory for durable flush: $Path")
    }
    try {
        if (-not [McpAgentMailInstallerNativeMethods]::FlushFileBuffers($handle)) {
            $errorCode = [Runtime.InteropServices.Marshal]::GetLastWin32Error()
            throw [ComponentModel.Win32Exception]::new($errorCode, "Could not durably flush directory: $Path")
        }
    } finally {
        $handle.Dispose()
    }
}

function Write-InstallerFileExclusive {
    param(
        [string]$Path,
        [byte[]]$Bytes
    )
    if (Test-InstallerEntryExists -Path $Path) {
        throw "Refusing to replace transaction entry: $Path"
    }
    $stream = [System.IO.FileStream]::new(
        $Path,
        [System.IO.FileMode]::CreateNew,
        [System.IO.FileAccess]::Write,
        [System.IO.FileShare]::None,
        4096,
        [System.IO.FileOptions]::WriteThrough
    )
    try {
        $stream.Write($Bytes, 0, $Bytes.Length)
        $stream.Flush($true)
    } finally {
        $stream.Dispose()
    }
    Assert-InstallerOwnedEntry -Path $Path -Label "Binary transaction file"
    Flush-InstallerDirectory -Path (Split-Path -LiteralPath $Path -Parent)
}

function Copy-InstallerFileExclusive {
    param(
        [string]$Source,
        [string]$Destination
    )
    if (Test-InstallerEntryExists -Path $Destination) {
        throw "Refusing to replace transaction payload: $Destination"
    }
    $inputStream = [System.IO.File]::Open(
        $Source, [System.IO.FileMode]::Open, [System.IO.FileAccess]::Read,
        [System.IO.FileShare]::Read
    )
    $outputStream = $null
    try {
        $outputStream = [System.IO.FileStream]::new(
            $Destination,
            [System.IO.FileMode]::CreateNew,
            [System.IO.FileAccess]::Write,
            [System.IO.FileShare]::None,
            65536,
            [System.IO.FileOptions]::WriteThrough
        )
        $inputStream.CopyTo($outputStream)
        $outputStream.Flush($true)
    } finally {
        if ($null -ne $outputStream) { $outputStream.Dispose() }
        $inputStream.Dispose()
    }
    Assert-InstallerOwnedEntry -Path $Destination -Label "Binary transaction payload"
    Flush-InstallerDirectory -Path (Split-Path -LiteralPath $Destination -Parent)
}

function Move-InstallerEntryNoReplaceDurable {
    param(
        [string]$Source,
        [string]$Destination,
        [string]$Label
    )
    if (-not (Test-InstallerEntryExists -Path $Source)) {
        throw "$Label source is missing: $Source"
    }
    if (Test-InstallerEntryExists -Path $Destination) {
        throw "$Label destination already exists: $Destination"
    }
    $sourceParent = Split-Path -LiteralPath $Source -Parent
    $destinationParent = Split-Path -LiteralPath $Destination -Parent
    if ([System.Environment]::OSVersion.Platform -eq [System.PlatformID]::Win32NT) {
        Initialize-InstallerNativeMethods
        # MOVEFILE_WRITE_THROUGH only. MOVEFILE_REPLACE_EXISTING is
        # intentionally omitted so an occupied path is never clobbered.
        if (-not [McpAgentMailInstallerNativeMethods]::MoveFileExW($Source, $Destination, [uint32]0x8)) {
            $errorCode = [Runtime.InteropServices.Marshal]::GetLastWin32Error()
            throw [ComponentModel.Win32Exception]::new($errorCode, "$Label failed")
        }
    } else {
        $attributes = [System.IO.File]::GetAttributes($Source)
        if (($attributes -band [System.IO.FileAttributes]::Directory) -ne 0) {
            [System.IO.Directory]::Move($Source, $Destination)
        } else {
            [System.IO.File]::Move($Source, $Destination)
        }
    }
    if ((Test-InstallerEntryExists -Path $Source) -or
        -not (Test-InstallerEntryExists -Path $Destination)) {
        throw "$Label did not satisfy no-replace move postconditions."
    }
    Flush-InstallerDirectory -Path $sourceParent
    if ($destinationParent -cne $sourceParent) {
        Flush-InstallerDirectory -Path $destinationParent
    }
}

function Assert-BinaryTransactionHash {
    param(
        [string]$Path,
        [string]$ExpectedHash,
        [string]$Label
    )
    Assert-InstallerOwnedEntry -Path $Path -Label $Label
    $actual = Get-Sha256Hex -FilePath $Path
    if ($actual -cne $ExpectedHash) {
        throw "$Label hash changed (expected $ExpectedHash, got $actual): $Path"
    }
}

function Get-BinaryTransactionActivePath {
    param([string]$InstallDir)
    return (Join-Path $InstallDir ".mcp-agent-mail-install-transaction.active")
}

function Write-BinaryTransactionPhase {
    param(
        [string]$Journal,
        [string]$Phase,
        [string]$MetadataHash
    )
    $validPhases = @(
        "00-prepared", "10-preserve-server", "20-preserve-cli",
        "30-publish-server", "40-publish-cli", "45-rollback", "50-commit-ready"
    )
    if ($Phase -cnotin $validPhases) {
        throw "Unknown binary transaction phase: $Phase"
    }
    $text = "phase=$Phase`nmetadata_sha256=$MetadataHash`n"
    $bytes = [Text.UTF8Encoding]::new($false).GetBytes($text)
    Write-InstallerFileExclusive -Path (Join-Path $Journal "phase.$Phase") -Bytes $bytes
}

function Read-BinaryTransactionMetadata {
    param([string]$Journal)
    Assert-InstallerOwnedEntry -Path $Journal -Label "Binary transaction authority" -Directory
    $metadataPath = Join-Path $Journal "metadata"
    $witnessPath = Join-Path $Journal "metadata.sha256"
    Assert-InstallerOwnedEntry -Path $metadataPath -Label "Binary transaction metadata"
    Assert-InstallerOwnedEntry -Path $witnessPath -Label "Binary transaction metadata witness"
    $utf8 = [Text.UTF8Encoding]::new($false, $true)
    $witnessText = [System.IO.File]::ReadAllText($witnessPath, $utf8)
    if ($witnessText -cnotmatch '^[a-f0-9]{64}\n$') {
        throw "Binary transaction metadata witness is malformed: $witnessPath"
    }
    $witness = $witnessText.Substring(0, 64)
    if ((Get-Sha256Hex -FilePath $metadataPath) -cne $witness) {
        throw "Binary transaction metadata hash witness does not match: $metadataPath"
    }
    $metadataText = [System.IO.File]::ReadAllText($metadataPath, $utf8)
    $lines = $metadataText.Split("`n")
    if ($lines.Count -ne 9 -or $lines[8] -cne "" -or $lines[0] -cne "schema=1") {
        throw "Binary transaction metadata is malformed: $metadataPath"
    }
    $expectedKeys = @(
        "nonce", "had_server", "old_server_sha256", "had_cli",
        "old_cli_sha256", "new_server_sha256", "new_cli_sha256"
    )
    $values = @{}
    for ($index = 0; $index -lt $expectedKeys.Count; $index++) {
        $prefix = $expectedKeys[$index] + "="
        $line = $lines[$index + 1]
        if (-not $line.StartsWith($prefix, [StringComparison]::Ordinal)) {
            throw "Binary transaction metadata key order is invalid: $metadataPath"
        }
        $values[$expectedKeys[$index]] = $line.Substring($prefix.Length)
    }
    if ($values.nonce -cnotmatch '^[A-Za-z0-9][A-Za-z0-9._-]{0,127}$') {
        throw "Binary transaction nonce is invalid."
    }
    foreach ($stem in @("server", "cli")) {
        $had = $values["had_$stem"]
        $oldHash = $values["old_${stem}_sha256"]
        if (($had -ceq "0" -and $oldHash -cne "absent") -or
            ($had -ceq "1" -and $oldHash -cnotmatch '^[a-f0-9]{64}$') -or
            ($had -cnotin @("0", "1"))) {
            throw "Binary transaction old-$stem witness is invalid."
        }
    }
    if ($values.new_server_sha256 -cnotmatch '^[a-f0-9]{64}$' -or
        $values.new_cli_sha256 -cnotmatch '^[a-f0-9]{64}$') {
        throw "Binary transaction new-binary witness is invalid."
    }
    return [pscustomobject]@{
        Nonce = $values.nonce
        HadServer = $values.had_server
        OldServerHash = $values.old_server_sha256
        HadCli = $values.had_cli
        OldCliHash = $values.old_cli_sha256
        NewServerHash = $values.new_server_sha256
        NewCliHash = $values.new_cli_sha256
        MetadataHash = $witness
    }
}

function Assert-BinaryTransactionPhaseMarker {
    param(
        [string]$Journal,
        [string]$Phase,
        [string]$MetadataHash
    )
    $path = Join-Path $Journal "phase.$Phase"
    Assert-InstallerOwnedEntry -Path $path -Label "Binary transaction phase marker"
    $actual = [System.IO.File]::ReadAllText($path, [Text.UTF8Encoding]::new($false, $true))
    $expected = "phase=$Phase`nmetadata_sha256=$MetadataHash`n"
    if ($actual -cne $expected) {
        throw "Binary transaction phase marker is malformed: $path"
    }
}

function Get-BinaryTransactionPhaseState {
    param(
        [string]$Journal,
        [string]$MetadataHash
    )
    $allowed = @{
        "metadata" = $true; "metadata.sha256" = $true
        "new-server" = $true; "new-cli" = $true
        "old-server" = $true; "old-cli" = $true
        "rollback-new-server" = $true; "rollback-new-cli" = $true
        "phase.00-prepared" = $true; "phase.10-preserve-server" = $true
        "phase.20-preserve-cli" = $true; "phase.30-publish-server" = $true
        "phase.40-publish-cli" = $true; "phase.45-rollback" = $true
        "phase.50-commit-ready" = $true
    }
    foreach ($entry in Get-ChildItem -LiteralPath $Journal -Force) {
        if (-not $allowed.ContainsKey($entry.Name)) {
            throw "Unexpected entry in binary transaction authority: $($entry.FullName)"
        }
    }
    $forward = @(
        "00-prepared", "10-preserve-server", "20-preserve-cli",
        "30-publish-server", "40-publish-cli", "50-commit-ready"
    )
    $latest = $null
    $missing = $false
    foreach ($phase in $forward) {
        $marker = Join-Path $Journal "phase.$phase"
        if (Test-InstallerEntryExists -Path $marker) {
            if ($missing) {
                throw "Binary transaction phase sequence has a gap before $phase."
            }
            Assert-BinaryTransactionPhaseMarker -Journal $Journal -Phase $phase -MetadataHash $MetadataHash
            $latest = $phase
        } else {
            $missing = $true
        }
    }
    if ($null -eq $latest) {
        throw "Binary transaction has no prepared phase marker."
    }
    $rollbackMarker = Join-Path $Journal "phase.45-rollback"
    $hasRollback = Test-InstallerEntryExists -Path $rollbackMarker
    if ($hasRollback) {
        Assert-BinaryTransactionPhaseMarker -Journal $Journal -Phase "45-rollback" -MetadataHash $MetadataHash
    }
    if ($hasRollback -and $latest -ceq "50-commit-ready") {
        throw "Binary transaction contains both rollback and commit-ready markers."
    }
    return [pscustomobject]@{ Forward = $latest; HasRollback = $hasRollback }
}

function Get-BinaryTransactionForwardTargetState {
    param(
        [string]$Journal,
        [string]$Destination,
        [string]$Stem,
        [string]$HadOriginal,
        [string]$OldHash,
        [string]$NewHash
    )
    $staged = Join-Path $Journal "new-$Stem"
    $backup = Join-Path $Journal "old-$Stem"
    $quarantined = Join-Path $Journal "rollback-new-$Stem"
    if (Test-InstallerEntryExists -Path $quarantined) {
        throw "Rollback residue exists without a rollback phase: $quarantined"
    }
    $hasStaged = Test-InstallerEntryExists -Path $staged
    $hasBackup = Test-InstallerEntryExists -Path $backup
    $hasDestination = Test-InstallerEntryExists -Path $Destination
    if ($hasStaged) { Assert-BinaryTransactionHash -Path $staged -ExpectedHash $NewHash -Label "Staged $Stem binary" }
    if ($hasBackup) {
        if ($HadOriginal -cne "1") { throw "Unexpected old-$Stem backup." }
        Assert-BinaryTransactionHash -Path $backup -ExpectedHash $OldHash -Label "Preserved $Stem binary"
    }
    $destinationHash = if ($hasDestination) {
        Assert-InstallerOwnedEntry -Path $Destination -Label "$Stem install target"
        Get-Sha256Hex -FilePath $Destination
    } else { "" }

    if ($hasStaged) {
        if ($hasBackup) {
            if ($hasDestination) { throw "Preserved $Stem destination is occupied." }
            return "preserved"
        }
        if ($HadOriginal -ceq "1") {
            if ($destinationHash -cne $OldHash) { throw "Original $Stem destination changed." }
            return "original"
        }
        if ($hasDestination) { throw "Absent $Stem destination appeared." }
        return "absent-unpublished"
    }
    if ($destinationHash -cne $NewHash) { throw "Published $Stem destination hash is invalid." }
    if (($HadOriginal -ceq "1") -ne $hasBackup) { throw "Published $Stem backup state is invalid." }
    return "published"
}

function Assert-BinaryTransactionForwardWindow {
    param(
        [string]$Journal,
        [string]$InstallDir,
        [pscustomobject]$Metadata,
        [string]$Phase
    )
    $serverState = Get-BinaryTransactionForwardTargetState `
        -Journal $Journal -Destination (Join-Path $InstallDir "mcp-agent-mail.exe") `
        -Stem "server" -HadOriginal $Metadata.HadServer `
        -OldHash $Metadata.OldServerHash -NewHash $Metadata.NewServerHash
    $cliState = Get-BinaryTransactionForwardTargetState `
        -Journal $Journal -Destination (Join-Path $InstallDir "am.exe") `
        -Stem "cli" -HadOriginal $Metadata.HadCli `
        -OldHash $Metadata.OldCliHash -NewHash $Metadata.NewCliHash
    $valid = switch ($Phase) {
        "00-prepared" {
            $serverState -cin @("original", "absent-unpublished") -and
                $cliState -cin @("original", "absent-unpublished")
        }
        "10-preserve-server" {
            $serverState -cin @("original", "preserved", "absent-unpublished") -and
                $cliState -cin @("original", "absent-unpublished")
        }
        "20-preserve-cli" {
            $serverState -cin @("preserved", "absent-unpublished") -and
                $cliState -cin @("original", "preserved", "absent-unpublished")
        }
        "30-publish-server" {
            $serverState -cin @("preserved", "published", "absent-unpublished") -and
                $cliState -cin @("preserved", "absent-unpublished")
        }
        "40-publish-cli" {
            $serverState -ceq "published" -and
                $cliState -cin @("preserved", "published", "absent-unpublished")
        }
        default { $false }
    }
    if (-not $valid) {
        throw "Binary transaction contents do not match phase $Phase (server=$serverState cli=$cliState)."
    }
}

function Restore-BinaryTransactionTarget {
    param(
        [string]$Journal,
        [string]$InstallDir,
        [string]$Stem,
        [string]$BinaryName,
        [string]$HadOriginal,
        [string]$OldHash,
        [string]$NewHash
    )
    $destination = Join-Path $InstallDir $BinaryName
    $staged = Join-Path $Journal "new-$Stem"
    $backup = Join-Path $Journal "old-$Stem"
    $quarantined = Join-Path $Journal "rollback-new-$Stem"
    $hasStaged = Test-InstallerEntryExists -Path $staged
    $hasBackup = Test-InstallerEntryExists -Path $backup
    $hasQuarantined = Test-InstallerEntryExists -Path $quarantined
    $hasDestination = Test-InstallerEntryExists -Path $destination
    if ($hasStaged) { Assert-BinaryTransactionHash -Path $staged -ExpectedHash $NewHash -Label "Staged $Stem binary" }
    if ($hasBackup) {
        if ($HadOriginal -cne "1") { throw "Unexpected old-$Stem backup." }
        Assert-BinaryTransactionHash -Path $backup -ExpectedHash $OldHash -Label "Preserved $Stem binary"
    }
    if ($hasQuarantined) {
        Assert-BinaryTransactionHash -Path $quarantined -ExpectedHash $NewHash -Label "Quarantined new $Stem binary"
        if ($hasStaged) { throw "Duplicate staged and quarantined new $Stem payloads." }
    }
    $destinationHash = if ($hasDestination) {
        Assert-InstallerOwnedEntry -Path $destination -Label "$Stem install target"
        Get-Sha256Hex -FilePath $destination
    } else { "" }

    if (-not $hasStaged -and -not $hasQuarantined) {
        if ($destinationHash -cne $NewHash) { throw "Rollback cannot identify the exact new $Stem destination." }
        if (($HadOriginal -ceq "1") -ne $hasBackup) { throw "Rollback $Stem backup state is invalid." }
        Move-InstallerEntryNoReplaceDurable -Source $destination -Destination $quarantined -Label "Quarantine new $Stem binary"
        $hasDestination = $false
        $destinationHash = ""
    }
    if ($hasStaged) {
        if ($hasBackup -and $hasDestination) { throw "Preserved $Stem destination is occupied." }
        if (-not $hasBackup -and $HadOriginal -ceq "1" -and $destinationHash -cne $OldHash) {
            throw "Original $Stem destination changed."
        }
        if ($HadOriginal -ceq "0" -and $hasDestination) { throw "Absent $Stem destination appeared." }
    }
    if ($HadOriginal -ceq "1") {
        if ($hasBackup) {
            if (Test-InstallerEntryExists -Path $destination) { throw "Rollback $Stem destination is occupied." }
            Move-InstallerEntryNoReplaceDurable -Source $backup -Destination $destination -Label "Restore old $Stem binary"
        } else {
            Assert-BinaryTransactionHash -Path $destination -ExpectedHash $OldHash -Label "Restored $Stem binary"
        }
    } elseif ((Test-InstallerEntryExists -Path $backup) -or (Test-InstallerEntryExists -Path $destination)) {
        throw "Rollback expected no original $Stem destination."
    }
}

function Archive-BinaryTransaction {
    param(
        [string]$Journal,
        [string]$InstallDir,
        [string]$Outcome,
        [string]$Nonce
    )
    if ($Outcome -cnotin @("committed", "rolled-back")) { throw "Invalid transaction outcome." }
    $history = Join-Path $InstallDir ".mcp-agent-mail-install-transaction.$Outcome.$Nonce"
    Move-InstallerEntryNoReplaceDurable -Source $Journal -Destination $history -Label "Archive $Outcome binary transaction"
    $script:ActiveBinaryTransactionInstallDir = $null
}

function Invoke-BinaryPairRecoveryCore {
    param(
        [string]$InstallDir,
        [string]$InterruptAfterPhaseForTest = ""
    )
    $journal = Get-BinaryTransactionActivePath -InstallDir $InstallDir
    if (-not (Test-InstallerEntryExists -Path $journal)) { return }
    $metadata = Read-BinaryTransactionMetadata -Journal $journal
    $phaseState = Get-BinaryTransactionPhaseState -Journal $journal -MetadataHash $metadata.MetadataHash
    if ($phaseState.Forward -ceq "50-commit-ready") {
        foreach ($unexpected in @("new-server", "new-cli", "rollback-new-server", "rollback-new-cli")) {
            if (Test-InstallerEntryExists -Path (Join-Path $journal $unexpected)) {
                throw "Commit-ready transaction contains unexpected payload: $unexpected"
            }
        }
        Assert-BinaryTransactionHash -Path (Join-Path $InstallDir "mcp-agent-mail.exe") `
            -ExpectedHash $metadata.NewServerHash -Label "Committed server binary"
        Assert-BinaryTransactionHash -Path (Join-Path $InstallDir "am.exe") `
            -ExpectedHash $metadata.NewCliHash -Label "Committed CLI binary"
        foreach ($entry in @(
            @{ Stem = "server"; Had = $metadata.HadServer; Hash = $metadata.OldServerHash },
            @{ Stem = "cli"; Had = $metadata.HadCli; Hash = $metadata.OldCliHash }
        )) {
            $backup = Join-Path $journal "old-$($entry.Stem)"
            if ($entry.Had -ceq "1") {
                Assert-BinaryTransactionHash -Path $backup -ExpectedHash $entry.Hash -Label "Committed $($entry.Stem) backup"
            } elseif (Test-InstallerEntryExists -Path $backup) {
                throw "Commit-ready transaction contains an unexpected old-$($entry.Stem) backup."
            }
        }
        Archive-BinaryTransaction -Journal $journal -InstallDir $InstallDir -Outcome "committed" -Nonce $metadata.Nonce
        return
    }
    if (-not $phaseState.HasRollback) {
        Assert-BinaryTransactionForwardWindow -Journal $journal -InstallDir $InstallDir `
            -Metadata $metadata -Phase $phaseState.Forward
        Write-BinaryTransactionPhase -Journal $journal -Phase "45-rollback" -MetadataHash $metadata.MetadataHash
        if ($InterruptAfterPhaseForTest -ceq "rollback-ready") {
            throw "injected interruption after durable rollback intent"
        }
    }
    Restore-BinaryTransactionTarget -Journal $journal -InstallDir $InstallDir -Stem "cli" `
        -BinaryName "am.exe" -HadOriginal $metadata.HadCli `
        -OldHash $metadata.OldCliHash -NewHash $metadata.NewCliHash
    Restore-BinaryTransactionTarget -Journal $journal -InstallDir $InstallDir -Stem "server" `
        -BinaryName "mcp-agent-mail.exe" -HadOriginal $metadata.HadServer `
        -OldHash $metadata.OldServerHash -NewHash $metadata.NewServerHash
    Archive-BinaryTransaction -Journal $journal -InstallDir $InstallDir -Outcome "rolled-back" -Nonce $metadata.Nonce
}

function Recover-BinaryPairTransaction {
    param(
        [string]$InstallDir,
        [string]$InterruptAfterPhaseForTest = ""
    )
    if ($script:BinaryTransactionRecoveryActive) {
        throw "Binary transaction recovery is already active."
    }
    $script:BinaryTransactionRecoveryActive = $true
    try {
        Invoke-BinaryPairRecoveryCore -InstallDir $InstallDir `
            -InterruptAfterPhaseForTest $InterruptAfterPhaseForTest
    } finally {
        $script:BinaryTransactionRecoveryActive = $false
    }
}

function New-BinaryPairTransaction {
    param(
        [string]$AmSource,
        [string]$ServerSource,
        [string]$InstallDir
    )
    $active = Get-BinaryTransactionActivePath -InstallDir $InstallDir
    if (Test-InstallerEntryExists -Path $active) { throw "An unrecovered binary transaction already exists: $active" }
    foreach ($source in @($AmSource, $ServerSource)) {
        Assert-InstallerOwnedEntry -Path $source -Label "Binary transaction source"
        if ((Get-Item -LiteralPath $source -Force).Length -le 0) { throw "Binary transaction source is empty: $source" }
    }
    $serverDestination = Join-Path $InstallDir "mcp-agent-mail.exe"
    $cliDestination = Join-Path $InstallDir "am.exe"
    foreach ($destination in @($serverDestination, $cliDestination)) {
        if (Test-InstallerEntryExists -Path $destination) {
            Assert-InstallerOwnedEntry -Path $destination -Label "Existing install target"
        }
    }
    $metadata = [ordered]@{
        Nonce = [Guid]::NewGuid().ToString("N")
        HadServer = if (Test-InstallerEntryExists -Path $serverDestination) { "1" } else { "0" }
        OldServerHash = "absent"
        HadCli = if (Test-InstallerEntryExists -Path $cliDestination) { "1" } else { "0" }
        OldCliHash = "absent"
        NewServerHash = Get-Sha256Hex -FilePath $ServerSource
        NewCliHash = Get-Sha256Hex -FilePath $AmSource
    }
    if ($metadata.HadServer -ceq "1") { $metadata.OldServerHash = Get-Sha256Hex -FilePath $serverDestination }
    if ($metadata.HadCli -ceq "1") { $metadata.OldCliHash = Get-Sha256Hex -FilePath $cliDestination }
    $preparing = Join-Path $InstallDir ".mcp-agent-mail-install-transaction.preparing.$($metadata.Nonce)"
    if (Test-InstallerEntryExists -Path $preparing) { throw "Preparing transaction path already exists: $preparing" }
    [System.IO.Directory]::CreateDirectory($preparing) | Out-Null
    Assert-InstallerOwnedEntry -Path $preparing -Label "Preparing binary transaction" -Directory
    Flush-InstallerDirectory -Path $InstallDir
    Copy-InstallerFileExclusive -Source $ServerSource -Destination (Join-Path $preparing "new-server")
    Copy-InstallerFileExclusive -Source $AmSource -Destination (Join-Path $preparing "new-cli")
    Assert-BinaryTransactionHash -Path (Join-Path $preparing "new-server") `
        -ExpectedHash $metadata.NewServerHash -Label "Journaled server binary"
    Assert-BinaryTransactionHash -Path (Join-Path $preparing "new-cli") `
        -ExpectedHash $metadata.NewCliHash -Label "Journaled CLI binary"
    $metadataText = @(
        "schema=1",
        "nonce=$($metadata.Nonce)",
        "had_server=$($metadata.HadServer)",
        "old_server_sha256=$($metadata.OldServerHash)",
        "had_cli=$($metadata.HadCli)",
        "old_cli_sha256=$($metadata.OldCliHash)",
        "new_server_sha256=$($metadata.NewServerHash)",
        "new_cli_sha256=$($metadata.NewCliHash)"
    ) -join "`n"
    $metadataText += "`n"
    $utf8 = [Text.UTF8Encoding]::new($false)
    Write-InstallerFileExclusive -Path (Join-Path $preparing "metadata") -Bytes $utf8.GetBytes($metadataText)
    $metadataHash = Get-Sha256Hex -FilePath (Join-Path $preparing "metadata")
    Write-InstallerFileExclusive -Path (Join-Path $preparing "metadata.sha256") `
        -Bytes $utf8.GetBytes("$metadataHash`n")
    Write-BinaryTransactionPhase -Journal $preparing -Phase "00-prepared" -MetadataHash $metadataHash
    Move-InstallerEntryNoReplaceDurable -Source $preparing -Destination $active `
        -Label "Publish binary transaction authority"
    Assert-InstallerOwnedEntry -Path $active -Label "Binary transaction authority" -Directory
    $script:ActiveBinaryTransactionInstallDir = $InstallDir
    return [pscustomobject]@{ Journal = $active; Metadata = (Read-BinaryTransactionMetadata -Journal $active) }
}

function Move-BinaryTransactionOriginal {
    param(
        [string]$Journal, [string]$InstallDir, [string]$Stem,
        [string]$BinaryName, [string]$HadOriginal, [string]$OldHash
    )
    $destination = Join-Path $InstallDir $BinaryName
    if ($HadOriginal -ceq "0") {
        if (Test-InstallerEntryExists -Path $destination) { throw "A $Stem destination appeared after preparation." }
        return
    }
    Assert-BinaryTransactionHash -Path $destination -ExpectedHash $OldHash -Label "Original $Stem binary"
    Move-InstallerEntryNoReplaceDurable -Source $destination -Destination (Join-Path $Journal "old-$Stem") `
        -Label "Preserve old $Stem binary"
}

function Move-BinaryTransactionNew {
    param(
        [string]$Journal, [string]$InstallDir, [string]$Stem,
        [string]$BinaryName, [string]$NewHash
    )
    $destination = Join-Path $InstallDir $BinaryName
    $staged = Join-Path $Journal "new-$Stem"
    if (Test-InstallerEntryExists -Path $destination) { throw "The $Stem destination is occupied before publication." }
    Assert-BinaryTransactionHash -Path $staged -ExpectedHash $NewHash -Label "Staged $Stem binary"
    Move-InstallerEntryNoReplaceDurable -Source $staged -Destination $destination -Label "Publish new $Stem binary"
}

function Install-BinariesAtomically {
    param(
        [string]$AmSource,
        [string]$ServerSource,
        [string]$InstallDir,
        [scriptblock]$PostInstallVerifier,
        [string]$InterruptAfterPhaseForTest = ""
    )
    $InstallDir = Assert-SafeInstallDirectory -InstallDir $InstallDir
    Recover-BinaryPairTransaction -InstallDir $InstallDir
    $leaveJournalForTest = $false
    try {
        $transaction = New-BinaryPairTransaction -AmSource $AmSource -ServerSource $ServerSource -InstallDir $InstallDir
        $journal = $transaction.Journal
        $metadata = $transaction.Metadata
        if ($InterruptAfterPhaseForTest -ceq "prepared") { $leaveJournalForTest = $true; throw "injected interruption" }
        Write-BinaryTransactionPhase -Journal $journal -Phase "10-preserve-server" -MetadataHash $metadata.MetadataHash
        if ($InterruptAfterPhaseForTest -ceq "preserve-server") { $leaveJournalForTest = $true; throw "injected interruption" }
        Move-BinaryTransactionOriginal -Journal $journal -InstallDir $InstallDir -Stem "server" `
            -BinaryName "mcp-agent-mail.exe" -HadOriginal $metadata.HadServer -OldHash $metadata.OldServerHash
        Write-BinaryTransactionPhase -Journal $journal -Phase "20-preserve-cli" -MetadataHash $metadata.MetadataHash
        if ($InterruptAfterPhaseForTest -ceq "preserve-cli") { $leaveJournalForTest = $true; throw "injected interruption" }
        Move-BinaryTransactionOriginal -Journal $journal -InstallDir $InstallDir -Stem "cli" `
            -BinaryName "am.exe" -HadOriginal $metadata.HadCli -OldHash $metadata.OldCliHash
        Write-BinaryTransactionPhase -Journal $journal -Phase "30-publish-server" -MetadataHash $metadata.MetadataHash
        if ($InterruptAfterPhaseForTest -ceq "publish-server") { $leaveJournalForTest = $true; throw "injected interruption" }
        Move-BinaryTransactionNew -Journal $journal -InstallDir $InstallDir -Stem "server" `
            -BinaryName "mcp-agent-mail.exe" -NewHash $metadata.NewServerHash
        Write-BinaryTransactionPhase -Journal $journal -Phase "40-publish-cli" -MetadataHash $metadata.MetadataHash
        if ($InterruptAfterPhaseForTest -ceq "publish-cli") { $leaveJournalForTest = $true; throw "injected interruption" }
        Move-BinaryTransactionNew -Journal $journal -InstallDir $InstallDir -Stem "cli" `
            -BinaryName "am.exe" -NewHash $metadata.NewCliHash
        if ((Get-Sha256Hex -FilePath (Join-Path $InstallDir "am.exe")) -cne $metadata.NewCliHash -or
            (Get-Sha256Hex -FilePath (Join-Path $InstallDir "mcp-agent-mail.exe")) -cne $metadata.NewServerHash) {
            throw "Installed binary bytes differ from the verified staged pair."
        }
        if ($null -ne $PostInstallVerifier) {
            & $PostInstallVerifier $InstallDir
        }
        Flush-InstallerDirectory -Path $InstallDir
        Write-BinaryTransactionPhase -Journal $journal -Phase "50-commit-ready" -MetadataHash $metadata.MetadataHash
        if ($InterruptAfterPhaseForTest -ceq "commit-ready") { $leaveJournalForTest = $true; throw "injected interruption" }
        Archive-BinaryTransaction -Journal $journal -InstallDir $InstallDir -Outcome "committed" -Nonce $metadata.Nonce
    } catch {
        $installError = $_.Exception.Message
        if ($leaveJournalForTest) { throw }
        try {
            Recover-BinaryPairTransaction -InstallDir $InstallDir
        } catch {
            $script:ActiveBinaryTransactionInstallDir = $InstallDir
            throw "Atomic binary replacement failed and recovery failed closed; active journal retained. Recovery error: $($_.Exception.Message). Root error: $installError"
        }
        throw "Atomic binary replacement failed. The previous binary pair was restored without deleting transaction evidence. Root error: $installError"
    }
}
            }
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

$Dest = Assert-SafeInstallDirectory -InstallDir $Dest
$installerMutex = Enter-InstallerMutex -InstallDir $Dest
$workDir = $null

try {
    if (-not $Force -and (Test-InstalledReleaseVersion -InstallDir $Dest -ExpectedVersion $requestedNormalized)) {
        Write-Info "mcp-agent-mail $resolvedVersion already reports the requested version at $Dest."
        Write-Info "Continuing with authenticated download and byte-for-byte replacement; a version string alone is not release provenance."
    }

    $workDir = Join-Path ([System.IO.Path]::GetTempPath()) ("mcp-agent-mail-install-" + [Guid]::NewGuid().ToString("N"))
    New-Item -ItemType Directory -Path $workDir | Out-Null
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
        Write-WarnText "The downloaded archive's binaries will execute for version checks before installation; malicious bytes can run arbitrary code."
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

    $postInstallVerifier = {
        param([string]$VerifiedInstallDir)
        Verify-Install -InstallDir $VerifiedInstallDir -ExpectedVersion $requestedNormalized
    }
    Install-BinariesAtomically `
        -AmSource $amSource `
        -ServerSource $serverSource `
        -InstallDir $Dest `
        -PostInstallVerifier $postInstallVerifier
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

} finally {
    if ($null -ne $workDir -and (Test-Path -LiteralPath $workDir)) {
        Remove-Item -LiteralPath $workDir -Recurse -Force -ErrorAction SilentlyContinue
    }
    Exit-InstallerMutex -Mutex $installerMutex
}

Write-Host ""
Write-Ok "mcp-agent-mail is installed."
Write-Host "Quick start:"
Write-Host "  am"
Write-Host "  am serve-http"
Write-Host "  mcp-agent-mail"
Write-Host "  am --help"
