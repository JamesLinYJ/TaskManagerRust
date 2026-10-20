param(
    [switch]$UseNightlyBuildStd,
    [ValidateSet(
        "x86_64-pc-windows-msvc",
        "i686-pc-windows-msvc",
        "aarch64-pc-windows-msvc"
    )]
    [string]$Target = "x86_64-pc-windows-msvc"
)

$ErrorActionPreference = "Stop"
$previousRustFlags = $env:RUSTFLAGS
$previousEncodedFlags = $env:CARGO_ENCODED_RUSTFLAGS
$separator = [char]0x1f

function Add-RemapPrefix {
    param(
        [System.Collections.Generic.List[string]]$Flags,
        [string]$From,
        [string]$To
    )

    if ([string]::IsNullOrWhiteSpace($From)) {
        return
    }

    $normalized = $From.TrimEnd('\', '/')
    if ([string]::IsNullOrWhiteSpace($normalized)) {
        return
    }

    $Flags.Add("--remap-path-prefix=$normalized=$To")
}

$repoRoot = Split-Path -Parent $PSScriptRoot
$rustflags = New-Object 'System.Collections.Generic.List[string]'

if ($null -ne $previousEncodedFlags) {
    foreach ($flag in $previousEncodedFlags.Split($separator)) {
        if ($flag.Length -gt 0) {
            $rustflags.Add($flag)
        }
    }
} elseif (-not [string]::IsNullOrWhiteSpace($previousRustFlags)) {
    # Match Cargo's whitespace-separated RUSTFLAGS semantics. The encoded variable takes
    # precedence when present, and keeps each new path-remapping argument intact.
    foreach ($flag in ($previousRustFlags -split '\s+')) {
        if ($flag.Length -gt 0) {
            $rustflags.Add($flag)
        }
    }
}

Add-RemapPrefix $rustflags $repoRoot "."
Add-RemapPrefix $rustflags $env:USERPROFILE "C:\\Users\\builder"
Add-RemapPrefix $rustflags $env:CARGO_HOME "C:\\.cargo"
Add-RemapPrefix $rustflags $env:RUSTUP_HOME "C:\\.rustup"

$sysroot = if ($UseNightlyBuildStd) {
    (rustup run nightly rustc --print sysroot).Trim()
} else {
    (rustc --print sysroot).Trim()
}
$sysrootAlias = if ($UseNightlyBuildStd) {
    "C:\\.rustup\\toolchains\\nightly"
} else {
    "C:\\.rustup\\toolchains\\stable"
}
Add-RemapPrefix $rustflags $sysroot $sysrootAlias

$rustSrc = Join-Path $sysroot "lib\\rustlib\\src\\rust"
if (Test-Path $rustSrc) {
    Add-RemapPrefix $rustflags $rustSrc "/rustc/src"
}

if ($UseNightlyBuildStd) {
    $rustflags.Add("-Zunstable-options")
    $rustflags.Add("-Cpanic=immediate-abort")
}
$env:CARGO_ENCODED_RUSTFLAGS = $rustflags -join $separator
Remove-Item Env:RUSTFLAGS -ErrorAction SilentlyContinue

Push-Location $repoRoot
try {
    if ($UseNightlyBuildStd) {
        $hasNightly = ((rustup toolchain list) | Select-String '^nightly').Length -gt 0
        if (-not $hasNightly) {
            throw "nightly toolchain is not installed"
        }

        cargo +nightly build `
            -Z build-std=std,panic_abort `
            --target $Target `
            --release `
            --locked
    } else {
        cargo build --target $Target --release --locked
    }
    if ($LASTEXITCODE -ne 0) {
        throw "cargo release build failed with exit code $LASTEXITCODE"
    }
} finally {
    Pop-Location
    if ($null -eq $previousEncodedFlags) {
        Remove-Item Env:CARGO_ENCODED_RUSTFLAGS -ErrorAction SilentlyContinue
    } else {
        $env:CARGO_ENCODED_RUSTFLAGS = $previousEncodedFlags
    }
    if ($null -eq $previousRustFlags) {
        Remove-Item Env:RUSTFLAGS -ErrorAction SilentlyContinue
    } else {
        $env:RUSTFLAGS = $previousRustFlags
    }
}
