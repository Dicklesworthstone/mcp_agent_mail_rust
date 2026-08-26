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
Load-InstallFunction "Get-Sha256Hex"
Load-InstallFunction "Assert-SafeInstallDirectory"
Load-InstallFunction "Install-BinariesAtomically"

$root = Join-Path ([System.IO.Path]::GetTempPath()) ("am-atomic-test-" + [Guid]::NewGuid().ToString("N"))
$srcDir = Join-Path $root "src"
$destDir = Join-Path $root "dest"
New-Item -ItemType Directory -Path $srcDir -Force | Out-Null
New-Item -ItemType Directory -Path $destDir -Force | Out-Null
try {
    $normalizedDest = Assert-SafeInstallDirectory -InstallDir ($destDir + [IO.Path]::DirectorySeparatorChar)
    if ($normalizedDest -cne $destDir) {
        throw "install directory normalization retained a trailing separator"
    }
    $amSrc = Join-Path $srcDir "am.exe"
    $serverSrc = Join-Path $srcDir "mcp-agent-mail.exe"
    Set-Content -LiteralPath $amSrc -Value "new-am" -NoNewline
    Set-Content -LiteralPath $serverSrc -Value "new-server" -NoNewline
    Set-Content -LiteralPath (Join-Path $destDir "am.exe") -Value "old-am" -NoNewline
    Set-Content -LiteralPath (Join-Path $destDir "mcp-agent-mail.exe") -Value "old-server" -NoNewline

    $contentVerifier = {
        param([string]$VerifiedInstallDir)
        if ((Get-Content -LiteralPath (Join-Path $VerifiedInstallDir "am.exe") -Raw) -cne "new-am") {
            throw "post-install am.exe content mismatch"
        }
        if ((Get-Content -LiteralPath (Join-Path $VerifiedInstallDir "mcp-agent-mail.exe") -Raw) -cne "new-server") {
            throw "post-install server content mismatch"
        }
    }
    Install-BinariesAtomically `
        -AmSource $amSrc `
        -ServerSource $serverSrc `
        -InstallDir $destDir `
        -PostInstallVerifier $contentVerifier

    if ((Get-Content -LiteralPath (Join-Path $destDir "am.exe") -Raw) -ne "new-am") {
        throw "am.exe was not atomically replaced"
    }
    if ((Get-Content -LiteralPath (Join-Path $destDir "mcp-agent-mail.exe") -Raw) -ne "new-server") {
        throw "mcp-agent-mail.exe was not atomically replaced"
    }
    $amDigestMatches = (Get-Sha256Hex -FilePath $amSrc) -ceq
        (Get-Sha256Hex -FilePath (Join-Path $destDir "am.exe"))
    $serverDigestMatches = (Get-Sha256Hex -FilePath $serverSrc) -ceq
        (Get-Sha256Hex -FilePath (Join-Path $destDir "mcp-agent-mail.exe"))
    if (-not $amDigestMatches -or -not $serverDigestMatches) {
        throw "installed bytes differ from staged bytes"
    }

    Set-Content -LiteralPath (Join-Path $destDir "am.exe") -Value "rollback-am" -NoNewline
    Set-Content -LiteralPath (Join-Path $destDir "mcp-agent-mail.exe") -Value "rollback-server" -NoNewline
    $threw = $false
    try {
        Install-BinariesAtomically `
            -AmSource $amSrc `
            -ServerSource $serverSrc `
            -InstallDir $destDir `
            -PostInstallVerifier $contentVerifier `
            -FailAfterFirstReplaceForTest
    } catch {
        $threw = $true
    }
    if (-not $threw) {
        throw "expected first-replacement fault to throw"
    }
    if ((Get-Content -LiteralPath (Join-Path $destDir "am.exe") -Raw) -cne "rollback-am" -or
        (Get-Content -LiteralPath (Join-Path $destDir "mcp-agent-mail.exe") -Raw) -cne "rollback-server") {
        throw "first-replacement fault did not restore the old pair"
    }

    $threw = $false
    try {
        Install-BinariesAtomically `
            -AmSource $amSrc `
            -ServerSource $serverSrc `
            -InstallDir $destDir `
            -PostInstallVerifier { throw "injected post-install verification failure" }
    } catch {
        $threw = $true
    }
    if (-not $threw) {
        throw "expected post-install verifier failure to throw"
    }
    if ((Get-Content -LiteralPath (Join-Path $destDir "am.exe") -Raw) -cne "rollback-am" -or
        (Get-Content -LiteralPath (Join-Path $destDir "mcp-agent-mail.exe") -Raw) -cne "rollback-server") {
        throw "post-install verifier failure did not restore the old pair"
    }

    $before = Get-Content -LiteralPath (Join-Path $destDir "am.exe") -Raw
    $missingSrc = Join-Path $srcDir "missing-server.exe"
    $threw = $false
    try {
        Install-BinariesAtomically -AmSource $amSrc -ServerSource $missingSrc -InstallDir $destDir
    } catch {
        $threw = $true
    }
    if (-not $threw) {
        throw "expected missing source to throw"
    }
    $after = Get-Content -LiteralPath (Join-Path $destDir "am.exe") -Raw
    if ($before -ne $after) {
        throw "destination mutated on missing source failure"
    }
    $backupResidue = @(Get-ChildItem -LiteralPath $destDir -Filter "*.bak.preinstall-*" -Force)
    if ($backupResidue.Count -ne 0) {
        throw "completed rollback retained unexpected backup residue: $($backupResidue.FullName -join ", ")"
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
