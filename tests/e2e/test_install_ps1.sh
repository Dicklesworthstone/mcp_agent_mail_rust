#!/usr/bin/env bash
# test_install_ps1.sh - Targeted behavioral checks for install.ps1 helper logic

E2E_SUITE="install_ps1"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../../scripts/e2e_lib.sh
source "${SCRIPT_DIR}/../../scripts/e2e_lib.sh"

e2e_init_artifacts
e2e_banner "PowerShell Installer Helper Suite"

if ! command -v pwsh >/dev/null 2>&1; then
    e2e_skip "pwsh not found; skipping install.ps1 helper checks"
    e2e_summary
    exit 0
fi

WORK="$(e2e_mktemp "e2e_install_ps1")"
INSTALL_PS1="${SCRIPT_DIR}/../../install.ps1"

run_pwsh_case() {
    local case_id="$1"
    local case_title="$2"
    local body="$3"
    local ps_file="${WORK}/${case_id}.ps1"
    local out_file="${WORK}/${case_id}.out"
    local out rc

    cat > "${ps_file}" <<'PS_SCRIPT'
param([string]$InstallPath)
Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"
PS_SCRIPT
    printf '%s\n' "$body" >> "${ps_file}"

    set +e
    pwsh -NoLogo -NoProfile -File "${ps_file}" "${INSTALL_PS1}" >"${out_file}" 2>&1
    rc=$?
    set -e

    out="$(cat "${out_file}" 2>/dev/null || true)"
    e2e_save_artifact "${case_id}.out.txt" "${out}"

    if [ "$rc" -eq 0 ]; then
        e2e_pass "${case_title}"
    else
        e2e_fail "${case_title}" "pwsh exit 0" "pwsh exit ${rc}"
        if [ -n "$out" ]; then
            printf '%s\n' "$out"
        fi
    fi
}

e2e_case_banner "1) install.ps1 parses cleanly"
run_pwsh_case \
    "case_01_parse" \
    "PowerShell parser has zero errors" \
'
$tokens = $null
$errors = $null
[void][System.Management.Automation.Language.Parser]::ParseFile(
    $InstallPath,
    [ref]$tokens,
    [ref]$errors
)
if ($errors.Count -gt 0) {
    $errors | ForEach-Object { Write-Error $_.Message }
    throw "parse errors detected"
}
'

e2e_case_banner "2) Release and Cosign version contracts are exact and fail closed"
run_pwsh_case \
    "case_02_version_contracts" \
    "Get-ReleaseContract and Assert-SafeCosignVersion reject ambiguous identities" \
'
$tokens = $null
$errors = $null
$ast = [System.Management.Automation.Language.Parser]::ParseFile(
    $InstallPath,
    [ref]$tokens,
    [ref]$errors
)
if ($errors.Count -gt 0) {
    throw "failed to parse install.ps1"
}
function Load-InstallFunction([string]$Name) {
    $fnAst = $ast.FindAll({
        param($node)
        $node -is [System.Management.Automation.Language.FunctionDefinitionAst]
    }, $true) | Where-Object { $_.Name -eq $Name } | Select-Object -First 1
    if ($null -eq $fnAst) {
        throw "function not found: $Name"
    }
    Invoke-Expression ("function global:{0} {1}" -f $Name, $fnAst.Body.Extent.Text)
}
Load-InstallFunction "Get-ReleaseContract"
Load-InstallFunction "Assert-SafeCosignVersion"

$release = Get-ReleaseContract -RawVersion "9.8.7-rc.1"
if ($release.Tag -cne "v9.8.7-rc.1" -or $release.Version -cne "9.8.7-rc.1") {
    throw "release contract did not normalize the exact tag"
}
$expectedIdentity = "https://github.com/Dicklesworthstone/mcp_agent_mail_rust/.github/workflows/dist.yml@refs/tags/v9.8.7-rc.1"
if ($release.CertificateIdentity -cne $expectedIdentity) {
    throw "release contract derived the wrong certificate identity"
}
foreach ($invalidRelease in @("v9.8.7+build", "release-v9.8.7", "v9.8.7/../../other")) {
    $threw = $false
    try { $null = Get-ReleaseContract -RawVersion $invalidRelease } catch { $threw = $true }
    if (-not $threw) {
        throw "invalid release identity was accepted: $invalidRelease"
    }
}

$safeCosignCases = @(
    @{ Output = @("GitVersion: v3.1.3"); Expected = "3.1.3" },
    @{ Output = @("GitVersion: v3.99.0"); Expected = "3.99.0" }
)
foreach ($case in $safeCosignCases) {
    $actual = Assert-SafeCosignVersion -VersionOutput $case.Output
    if ($actual.ToString() -cne $case.Expected) {
        throw "safe cosign version mismatch: expected $($case.Expected), got $actual"
    }
}
$unsafeCosignCases = @(
    @{ Output = @("GitVersion: v3.1.2") },
    @{ Output = @("GitVersion: v2.6.5") },
    @{ Output = @("GitVersion: v4.0.0") },
    @{ Output = @("GitVersion: v3.1.3-rc.1") },
    @{ Output = @("GitVersion: v3.1.3", "GitVersion: v3.2.0") },
    @{ Output = @("cosign version 3.1.3") }
)
foreach ($case in $unsafeCosignCases) {
    $threw = $false
    try { $null = Assert-SafeCosignVersion -VersionOutput $case.Output } catch { $threw = $true }
    if (-not $threw) {
        throw "unsafe or ambiguous cosign output was accepted: $($case.Output -join "; ")"
    }
}
'

e2e_case_banner "3) Checksum helpers validate good hash and reject bad hash"
run_pwsh_case \
    "case_03_checksum" \
    "Verify-ChecksumFile succeeds on matching hash and fails on mismatch" \
'
$tokens = $null
$errors = $null
$ast = [System.Management.Automation.Language.Parser]::ParseFile(
    $InstallPath,
    [ref]$tokens,
    [ref]$errors
)
if ($errors.Count -gt 0) {
    throw "failed to parse install.ps1"
}
function Load-InstallFunction([string]$Name) {
    $fnAst = $ast.FindAll({
        param($node)
        $node -is [System.Management.Automation.Language.FunctionDefinitionAst]
    }, $true) | Where-Object { $_.Name -eq $Name } | Select-Object -First 1
    if ($null -eq $fnAst) {
        throw "function not found: $Name"
    }
    Invoke-Expression ("function global:{0} {1}" -f $Name, $fnAst.Body.Extent.Text)
}
Load-InstallFunction "Write-Ok"
Load-InstallFunction "Get-Sha256Hex"
Load-InstallFunction "Parse-ChecksumHex"
Load-InstallFunction "Verify-ChecksumFile"

$tmp = [System.IO.Path]::GetTempFileName()
try {
    Set-Content -LiteralPath $tmp -Value "checksum-test" -NoNewline
    $good = Get-Sha256Hex -FilePath $tmp
    Verify-ChecksumFile -FilePath $tmp -ExpectedChecksum $good

    $threw = $false
    try {
        Verify-ChecksumFile -FilePath $tmp -ExpectedChecksum ("0" * 64)
    } catch {
        $threw = $true
    }
    if (-not $threw) {
        throw "expected checksum mismatch to throw"
    }
} finally {
    if (Test-Path -LiteralPath $tmp) {
        Remove-Item -LiteralPath $tmp -Force -ErrorAction SilentlyContinue
    }
}
'

e2e_case_banner "4) Pair transaction preserves bytes and rolls back every failure state"
run_pwsh_case \
    "case_04_atomic" \
    "Install-BinariesAtomically commits only after post-check and restores the old pair" \
'
$tokens = $null
$errors = $null
$ast = [System.Management.Automation.Language.Parser]::ParseFile(
    $InstallPath,
    [ref]$tokens,
    [ref]$errors
)
if ($errors.Count -gt 0) {
    throw "failed to parse install.ps1"
}
function Load-InstallFunction([string]$Name) {
    $fnAst = $ast.FindAll({
        param($node)
        $node -is [System.Management.Automation.Language.FunctionDefinitionAst]
    }, $true) | Where-Object { $_.Name -eq $Name } | Select-Object -First 1
    if ($null -eq $fnAst) {
        throw "function not found: $Name"
    }
    Invoke-Expression ("function global:{0} {1}" -f $Name, $fnAst.Body.Extent.Text)
}
foreach ($name in @(
    "Get-Sha256Hex",
    "Assert-SafeInstallDirectory",
    "Initialize-InstallerNativeMethods",
    "Test-InstallerEntryExists",
    "Get-InstallerLinkCount",
    "Assert-InstallerOwnedEntry",
    "New-InstallerDirectoryNoReplace",
    "Write-InstallerFileExclusive",
    "Copy-InstallerFileExclusive",
    "Move-InstallerEntryNoReplaceDurable",
    "Assert-BinaryTransactionHash",
    "Get-BinaryTransactionActivePath",
    "Write-BinaryTransactionPhase",
    "Read-BinaryTransactionMetadata",
    "Assert-BinaryTransactionPhaseMarker",
    "Get-BinaryTransactionPhaseState",
    "Get-BinaryTransactionForwardTargetState",
    "Assert-BinaryTransactionForwardWindow",
    "Restore-BinaryTransactionTarget",
    "Archive-BinaryTransaction",
    "Invoke-BinaryPairRecoveryCore",
    "Recover-BinaryPairTransaction",
    "New-BinaryPairTransaction",
    "Move-BinaryTransactionOriginal",
    "Move-BinaryTransactionNew",
    "Install-BinariesAtomically"
)) {
    Load-InstallFunction $name
}

$root = Join-Path ([System.IO.Path]::GetTempPath()) ("am-atomic-test-" + [Guid]::NewGuid().ToString("N"))
$srcDir = Join-Path $root "src"
New-Item -ItemType Directory -Path $srcDir -Force | Out-Null
try {
    $script:ActiveBinaryTransactionInstallDir = $null
    $script:BinaryTransactionRecoveryActive = $false
    $amSrc = Join-Path $srcDir "am.exe"
    $serverSrc = Join-Path $srcDir "mcp-agent-mail.exe"
    Set-Content -LiteralPath $amSrc -Value "new-am" -NoNewline
    Set-Content -LiteralPath $serverSrc -Value "new-server" -NoNewline

    $contentVerifier = {
        param([string]$VerifiedInstallDir)
        if ((Get-Content -LiteralPath (Join-Path $VerifiedInstallDir "am.exe") -Raw) -cne "new-am") {
            throw "post-install am.exe content mismatch"
        }
        if ((Get-Content -LiteralPath (Join-Path $VerifiedInstallDir "mcp-agent-mail.exe") -Raw) -cne "new-server") {
            throw "post-install server content mismatch"
        }
    }

    function New-TransactionCase([string]$Name, [bool]$Upgrade) {
        $caseDir = Join-Path $root $Name
        New-Item -ItemType Directory -Path $caseDir -Force | Out-Null
        if ($Upgrade) {
            Set-Content -LiteralPath (Join-Path $caseDir "am.exe") -Value "old-am" -NoNewline
            Set-Content -LiteralPath (Join-Path $caseDir "mcp-agent-mail.exe") -Value "old-server" -NoNewline
        }
        return $caseDir
    }

    function Assert-OldPair([string]$CaseDir) {
        if ((Get-Content -LiteralPath (Join-Path $CaseDir "am.exe") -Raw) -cne "old-am" -or
            (Get-Content -LiteralPath (Join-Path $CaseDir "mcp-agent-mail.exe") -Raw) -cne "old-server") {
            throw "case did not converge to old-old: $CaseDir"
        }
    }

    function Assert-NewPair([string]$CaseDir) {
        if ((Get-Content -LiteralPath (Join-Path $CaseDir "am.exe") -Raw) -cne "new-am" -or
            (Get-Content -LiteralPath (Join-Path $CaseDir "mcp-agent-mail.exe") -Raw) -cne "new-server") {
            throw "case did not converge to new-new: $CaseDir"
        }
    }

    $interruptions = @(
        "prepared",
        "preserve-server", "preserve-server-moved",
        "preserve-cli", "preserve-cli-moved",
        "publish-server", "publish-server-moved",
        "publish-cli", "publish-cli-moved",
        "commit-ready"
    )
    foreach ($kind in @("upgrade", "fresh")) {
        foreach ($phase in $interruptions) {
            $caseDir = New-TransactionCase -Name "$kind-$phase" -Upgrade ($kind -ceq "upgrade")
            $threw = $false
            try {
                Install-BinariesAtomically `
                    -AmSource $amSrc -ServerSource $serverSrc -InstallDir $caseDir `
                    -PostInstallVerifier $contentVerifier -InterruptAfterPhaseForTest $phase
            } catch { $threw = $true }
            if (-not $threw) { throw "$kind/$phase interruption did not throw" }
            $active = Get-BinaryTransactionActivePath -InstallDir $caseDir
            if (-not (Test-InstallerEntryExists -Path $active)) { throw "$kind/$phase did not retain active authority" }
            Recover-BinaryPairTransaction -InstallDir $caseDir
            if (Test-InstallerEntryExists -Path $active) { throw "$kind/$phase did not archive active authority" }
            if ($phase -ceq "commit-ready") {
                Assert-NewPair -CaseDir $caseDir
                $outcome = "committed"
            } else {
                if ($kind -ceq "upgrade") {
                    Assert-OldPair -CaseDir $caseDir
                } elseif ((Test-InstallerEntryExists -Path (Join-Path $caseDir "am.exe")) -or
                          (Test-InstallerEntryExists -Path (Join-Path $caseDir "mcp-agent-mail.exe"))) {
                    throw "fresh/$phase did not converge to absent-absent"
                }
                $outcome = "rolled-back"
            }
            $history = @(Get-ChildItem -LiteralPath $caseDir -Force |
                Where-Object { $_.Name -like ".mcp-agent-mail-install-transaction.$outcome.*" })
            if ($history.Count -ne 1) { throw "$kind/$phase did not retain exactly one $outcome journal" }
            Recover-BinaryPairTransaction -InstallDir $caseDir
        }
    }

    $normalDir = New-TransactionCase -Name "normal" -Upgrade $true
    Install-BinariesAtomically -AmSource $amSrc -ServerSource $serverSrc `
        -InstallDir $normalDir -PostInstallVerifier $contentVerifier
    Assert-NewPair -CaseDir $normalDir

    foreach ($fault in @("partial", "publish-boundary")) {
        $caseDir = New-TransactionCase -Name "marker-$fault" -Upgrade $true
        try {
            Install-BinariesAtomically -AmSource $amSrc -ServerSource $serverSrc `
                -InstallDir $caseDir -PostInstallVerifier $contentVerifier `
                -InterruptAfterPhaseForTest "prepared"
        } catch { }
        $active = Get-BinaryTransactionActivePath -InstallDir $caseDir
        $metadata = Read-BinaryTransactionMetadata -Journal $active
        $threw = $false
        try {
            if ($fault -ceq "partial") {
                Write-BinaryTransactionPhase -Journal $active -Phase "10-preserve-server" `
                    -MetadataHash $metadata.MetadataHash -PartialBeforePublishForTest
            } else {
                Write-BinaryTransactionPhase -Journal $active -Phase "10-preserve-server" `
                    -MetadataHash $metadata.MetadataHash -InterruptBeforePublishForTest
            }
        } catch { $threw = $true }
        if (-not $threw) { throw "$fault marker interruption did not throw" }
        if (Test-InstallerEntryExists -Path (Join-Path $active "phase.10-preserve-server")) {
            throw "$fault marker became authoritative before publication"
        }
        Recover-BinaryPairTransaction -InstallDir $caseDir
        Assert-OldPair -CaseDir $caseDir
    }

    $rollbackDir = New-TransactionCase -Name "rollback-interrupt" -Upgrade $true
    try {
        Install-BinariesAtomically -AmSource $amSrc -ServerSource $serverSrc `
            -InstallDir $rollbackDir -PostInstallVerifier $contentVerifier `
            -InterruptAfterPhaseForTest "publish-server-moved"
    } catch { }
    $threw = $false
    try {
        Recover-BinaryPairTransaction -InstallDir $rollbackDir -InterruptAfterPhaseForTest "rollback-ready"
    } catch { $threw = $true }
    if (-not $threw) { throw "rollback interruption did not throw" }
    Recover-BinaryPairTransaction -InstallDir $rollbackDir
    Assert-OldPair -CaseDir $rollbackDir
    Recover-BinaryPairTransaction -InstallDir $rollbackDir

    $verifyDir = New-TransactionCase -Name "verify-failure" -Upgrade $true
    $threw = $false
    try {
        Install-BinariesAtomically -AmSource $amSrc -ServerSource $serverSrc `
            -InstallDir $verifyDir -PostInstallVerifier { throw "injected verifier failure" }
    } catch { $threw = $true }
    if (-not $threw) { throw "post-install verifier failure did not throw" }
    Assert-OldPair -CaseDir $verifyDir

    $corruptDir = New-TransactionCase -Name "corrupt-journal" -Upgrade $true
    try {
        Install-BinariesAtomically -AmSource $amSrc -ServerSource $serverSrc `
            -InstallDir $corruptDir -PostInstallVerifier $contentVerifier `
            -InterruptAfterPhaseForTest "prepared"
    } catch { }
    $corruptActive = Get-BinaryTransactionActivePath -InstallDir $corruptDir
    Add-Content -LiteralPath (Join-Path $corruptActive "metadata") -Value "tampered"
    $threw = $false
    try { Recover-BinaryPairTransaction -InstallDir $corruptDir } catch { $threw = $true }
    if (-not $threw) { throw "corrupted journal did not fail closed" }
    Assert-OldPair -CaseDir $corruptDir
    if (-not (Test-InstallerEntryExists -Path $corruptActive)) { throw "corrupted active journal was not retained" }

    $unexpectedDir = New-TransactionCase -Name "unexpected-destination" -Upgrade $true
    try {
        Install-BinariesAtomically -AmSource $amSrc -ServerSource $serverSrc `
            -InstallDir $unexpectedDir -PostInstallVerifier $contentVerifier `
            -InterruptAfterPhaseForTest "publish-server"
    } catch { }
    Set-Content -LiteralPath (Join-Path $unexpectedDir "mcp-agent-mail.exe") `
        -Value "user-modified-server" -NoNewline
    $unexpectedActive = Get-BinaryTransactionActivePath -InstallDir $unexpectedDir
    $threw = $false
    try { Recover-BinaryPairTransaction -InstallDir $unexpectedDir } catch { $threw = $true }
    if (-not $threw) { throw "unexpected destination did not fail closed" }
    if ((Get-Content -LiteralPath (Join-Path $unexpectedDir "mcp-agent-mail.exe") -Raw) -cne "user-modified-server") {
        throw "recovery clobbered unexpected destination bytes"
    }
    if (-not (Test-InstallerEntryExists -Path $unexpectedActive)) { throw "ambiguous active journal was not retained" }

    $missingDir = New-TransactionCase -Name "missing-source" -Upgrade $true
    $before = Get-Content -LiteralPath (Join-Path $missingDir "am.exe") -Raw
    $missingSrc = Join-Path $srcDir "missing-server.exe"
    $threw = $false
    try {
        Install-BinariesAtomically -AmSource $amSrc -ServerSource $missingSrc -InstallDir $missingDir
    } catch { $threw = $true }
    if (-not $threw) { throw "missing source did not throw" }
    $after = Get-Content -LiteralPath (Join-Path $missingDir "am.exe") -Raw
    if ($before -cne $after) { throw "missing source failure mutated destination" }
    if (Test-InstallerEntryExists -Path (Get-BinaryTransactionActivePath -InstallDir $missingDir)) {
        throw "missing source failure left an active authority"
    }
} finally {
    if (Test-Path -LiteralPath $root) {
        Remove-Item -LiteralPath $root -Recurse -Force -ErrorAction SilentlyContinue
    }
}
'

e2e_case_banner "5) Sigstore verification isolates trust and forces the modern parser"
run_pwsh_case \
    "case_05_sigstore_isolation" \
    "Verify-SigstoreBundle clears custom trust only for cosign and restores caller state" \
'
$tokens = $null
$errors = $null
$ast = [System.Management.Automation.Language.Parser]::ParseFile(
    $InstallPath,
    [ref]$tokens,
    [ref]$errors
)
if ($errors.Count -gt 0) {
    throw "failed to parse install.ps1"
}
function Load-InstallFunction([string]$Name) {
    $fnAst = $ast.FindAll({
        param($node)
        $node -is [System.Management.Automation.Language.FunctionDefinitionAst]
    }, $true) | Where-Object { $_.Name -eq $Name } | Select-Object -First 1
    if ($null -eq $fnAst) {
        throw "function not found: $Name"
    }
    Invoke-Expression ("function global:{0} {1}" -f $Name, $fnAst.Body.Extent.Text)
}
foreach ($name in @(
    "Write-Info",
    "Write-Ok",
    "Verify-SigstoreBundle"
)) {
    Load-InstallFunction $name
}

$root = Join-Path ([System.IO.Path]::GetTempPath()) ("am-sigstore-test-" + [Guid]::NewGuid().ToString("N"))
$workDir = Join-Path $root "work"
New-Item -ItemType Directory -Path $workDir -Force | Out-Null
try {
    function global:Get-SafeCosignPath { return "Invoke-FakeCosign" }
    function global:Invoke-FakeCosign {
        $global:ObservedCosignArgs = @($args)
        foreach ($name in @("SIGSTORE_ROOT_FILE", "SIGSTORE_REKOR_PUBLIC_KEY", "SIGSTORE_CT_LOG_PUBLIC_KEY_FILE")) {
            if (-not [string]::IsNullOrEmpty([Environment]::GetEnvironmentVariable($name, "Process"))) {
                throw "custom trust environment leaked: $name"
            }
        }
        $global:LASTEXITCODE = 0
    }
    $trustValues = @{
        SIGSTORE_ROOT_FILE = "attacker-root"
        SIGSTORE_REKOR_PUBLIC_KEY = "attacker-rekor"
        SIGSTORE_CT_LOG_PUBLIC_KEY_FILE = "attacker-ctfe"
    }
    foreach ($entry in $trustValues.GetEnumerator()) {
        [Environment]::SetEnvironmentVariable($entry.Key, $entry.Value, "Process")
    }

    function global:Download-File {
        param([string]$Url, [string]$OutFile)
        Set-Content -LiteralPath $OutFile -Value "{`"mediaType`":`"application/vnd.dev.sigstore.bundle.v0.3+json`"}" -NoNewline
    }
    $artifact = Join-Path $root "release.zip"
    Set-Content -LiteralPath $artifact -Value "artifact" -NoNewline
    $global:CosignIdentity = "https://github.com/Dicklesworthstone/mcp_agent_mail_rust/.github/workflows/dist.yml@refs/tags/v9.9.9"
    $global:CosignOidcIssuer = "https://token.actions.githubusercontent.com"

    Verify-SigstoreBundle `
        -FilePath $artifact `
        -AssetUrl "https://example.invalid/release.zip" `
        -WorkDir $workDir

    foreach ($entry in $trustValues.GetEnumerator()) {
        $restored = [Environment]::GetEnvironmentVariable($entry.Key, "Process")
        if ($restored -cne $entry.Value) {
            throw "trust environment was not restored: $($entry.Key)"
        }
    }
    $cosignArgs = $global:ObservedCosignArgs -join [Environment]::NewLine
    if ($cosignArgs -notmatch "(?m)^--new-bundle-format$") {
        throw "cosign did not receive --new-bundle-format"
    }
    if ($cosignArgs -notmatch [regex]::Escape($global:CosignIdentity)) {
        throw "cosign did not receive the exact certificate identity"
    }
} finally {
    if (Test-Path -LiteralPath $root) {
        Remove-Item -LiteralPath $root -Recurse -Force -ErrorAction SilentlyContinue
    }
}
'

e2e_summary
