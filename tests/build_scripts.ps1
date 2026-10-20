# +-------------------------------------------------------------------------
#
#   taskmgr-rs - Build script failure and argument-boundary regression tests
#
#   File:        tests/build_scripts.ps1
#   Date:        2026-10-20
#   Environment: Linux 6.18.55 x86_64; PowerShell 7.6.7; Rust 1.99.1
#   Author:      OpenAI Codex
# --------------------------------------------------------------------------

# Run the production scripts in a disposable path containing spaces. The fake Cargo exits
# unsuccessfully after recording its environment; no real build or user artifact is touched.
$ErrorActionPreference = 'Stop'
$repoRoot = Split-Path -Parent $PSScriptRoot
$fixture = Join-Path ([IO.Path]::GetTempPath()) "taskmgr build scripts $([guid]::NewGuid())"
$separator = [char]0x1f
$savedEnvironment = @{}
foreach ($name in @('CARGO', 'RUSTFLAGS', 'CARGO_ENCODED_RUSTFLAGS', 'TASKMGR_BUILD_SCRIPT_CAPTURE')) {
    $savedEnvironment[$name] = [Environment]::GetEnvironmentVariable($name, 'Process')
}

function Assert-True {
    param([bool]$Condition, [string]$Message)
    if (-not $Condition) {
        throw $Message
    }
}

function Invoke-ExpectedBuildFailure {
    param([string]$ScriptPath)
    $message = $null
    try {
        & $ScriptPath -Target x86_64-pc-windows-msvc
    } catch {
        $message = $_.Exception.Message
    }
    Assert-True ($null -ne $message -and $message -match 'build failed with exit code 17') `
        "Expected Cargo failure to abort the script; got: $message"
}

try {
    $scripts = New-Item -ItemType Directory -Path (Join-Path $fixture 'scripts') -Force
    foreach ($name in @('release-clean.ps1', 'build-diagnostics.ps1')) {
        Copy-Item -LiteralPath (Join-Path $repoRoot "scripts/$name") -Destination $scripts.FullName
    }
    $fakeSysroot = (New-Item -ItemType Directory -Path (Join-Path $fixture 'tool chain')).FullName
    $fakeCargo = Join-Path $fixture 'fake-cargo.ps1'
    @'
param([Parameter(ValueFromRemainingArguments)][string[]]$Arguments)
[pscustomobject]@{
    arguments = $Arguments
    encoded = $env:CARGO_ENCODED_RUSTFLAGS
    legacy = $env:RUSTFLAGS
} | ConvertTo-Json -Depth 4 | Set-Content -LiteralPath $env:TASKMGR_BUILD_SCRIPT_CAPTURE
exit 17
'@ | Set-Content -LiteralPath $fakeCargo

    function cargo { & $fakeCargo @args }
    function rustc {
        $fakeSysroot
        $global:LASTEXITCODE = 0
    }
    $env:CARGO = $fakeCargo
    $env:TASKMGR_BUILD_SCRIPT_CAPTURE = Join-Path $fixture 'captured.json'

    $diagnostics = New-Item -ItemType Directory `
        -Path (Join-Path $fixture 'target/x86_64-pc-windows-msvc/diagnostics') -Force
    $oldHash = Join-Path $diagnostics.FullName 'taskmgr.exe.sha256'
    [IO.File]::WriteAllText((Join-Path $diagnostics.FullName 'taskmgr.exe'), 'old executable')
    [IO.File]::WriteAllText((Join-Path $diagnostics.FullName 'taskmgr.pdb'), 'old symbols')
    [IO.File]::WriteAllText($oldHash, 'old hash')
    $env:RUSTFLAGS = '--cfg diagnostic_original'
    $env:CARGO_ENCODED_RUSTFLAGS = '--cfg' + $separator + 'diagnostic_encoded'
    Invoke-ExpectedBuildFailure (Join-Path $scripts.FullName 'build-diagnostics.ps1')
    Assert-True ([IO.File]::ReadAllText($oldHash) -ceq 'old hash') `
        'A failed diagnostic build overwrote the hash using stale artifacts'
    Assert-True ($env:RUSTFLAGS -ceq '--cfg diagnostic_original') 'Diagnostic RUSTFLAGS were not restored'
    Assert-True ($env:CARGO_ENCODED_RUSTFLAGS -ceq ('--cfg' + $separator + 'diagnostic_encoded')) `
        'Diagnostic encoded flags were not restored'
    $captured = Get-Content -Raw -LiteralPath $env:TASKMGR_BUILD_SCRIPT_CAPTURE | ConvertFrom-Json
    Assert-True ($captured.arguments -contains '--locked') 'Diagnostic build did not preserve Cargo.lock'
    Write-Host 'PASS: diagnostic failure rejects stale EXE/PDB and restores environment'

    $env:RUSTFLAGS = '--cfg inherited_flag -C opt-level=2'
    $env:CARGO_ENCODED_RUSTFLAGS = $null
    Invoke-ExpectedBuildFailure (Join-Path $scripts.FullName 'release-clean.ps1')
    $captured = Get-Content -Raw -LiteralPath $env:TASKMGR_BUILD_SCRIPT_CAPTURE | ConvertFrom-Json
    $flags = @($captured.encoded.Split($separator))
    Assert-True ($flags -contains "--remap-path-prefix=$fixture=.") `
        'The repository path with spaces was not passed as one rustc argument'
    Assert-True ($flags -contains "--remap-path-prefix=$fakeSysroot=C:\\.rustup\\toolchains\\stable") `
        'The toolchain path with spaces was not passed as one rustc argument'
    Assert-True (($flags -contains 'inherited_flag') -and ($flags -contains 'opt-level=2')) `
        'Existing legacy flags were lost'
    Assert-True ([string]::IsNullOrEmpty($captured.legacy)) 'Legacy flags should not compete with encoded flags'
    Assert-True ($env:RUSTFLAGS -ceq '--cfg inherited_flag -C opt-level=2') 'Release RUSTFLAGS were not restored'
    Assert-True ([string]::IsNullOrEmpty($env:CARGO_ENCODED_RUSTFLAGS)) 'Release encoded flags leaked'
    Write-Host 'PASS: release keeps paths with spaces as single arguments'

    $env:RUSTFLAGS = '--cfg ignored_legacy'
    $originalEncoded = '--cfg' + $separator + 'name="two words"'
    $env:CARGO_ENCODED_RUSTFLAGS = $originalEncoded
    Invoke-ExpectedBuildFailure (Join-Path $scripts.FullName 'release-clean.ps1')
    $captured = Get-Content -Raw -LiteralPath $env:TASKMGR_BUILD_SCRIPT_CAPTURE | ConvertFrom-Json
    $flags = @($captured.encoded.Split($separator))
    Assert-True ($flags -contains 'name="two words"') 'Existing encoded argument was split or lost'
    Assert-True (-not ($flags -contains 'ignored_legacy')) 'Legacy flags overrode Cargo encoded precedence'
    Assert-True ($flags -contains "--remap-path-prefix=$fixture=.") `
        'Existing encoded flags prevented the release remapping arguments'
    Assert-True ($env:CARGO_ENCODED_RUSTFLAGS -ceq $originalEncoded) 'Original encoded flags were not restored'
    Assert-True ($env:RUSTFLAGS -ceq '--cfg ignored_legacy') 'Original legacy flags were not restored'
    Write-Host 'PASS: release preserves encoded flags and appends remapping arguments'
} finally {
    foreach ($name in $savedEnvironment.Keys) {
        [Environment]::SetEnvironmentVariable($name, $savedEnvironment[$name], 'Process')
    }
    if (Test-Path -LiteralPath $fixture) {
        Remove-Item -LiteralPath $fixture -Recurse -Force
    }
}

# GitHub Actions' pwsh wrapper propagates LASTEXITCODE. The deliberate failures above are
# successful test outcomes, so they must not become the status of this completed test script.
$global:LASTEXITCODE = 0
