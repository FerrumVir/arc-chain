<#
.SYNOPSIS
ARC Proof Kit for Windows: check, on this computer, that the public SmolLM3-3B
model gives the published answer bit for bit (docs/proof-kit.md).

.DESCRIPTION
Usage:
  powershell -ExecutionPolicy Bypass -File scripts\proof-kit\run.ps1 [--bin PATH | --release TAG] [kit options]

Kit options (passed to `arc-modern proof`):
  --dry-run                print the exact JSON a submission would send; send nothing
  --submit --endpoint URL  send the result to a Hash Wall, after you see it and type yes
  --dir DIR                where the model and results live (default: %LOCALAPPDATA%\arc-proof-kit)
  --backends LIST          cpu-scalar,cpu-simd (default: every kernel this CPU has)
  --threads N  --keep-source  --no-island  --force  --gpu

Where arc-modern comes from (first match wins):
  --bin PATH, or ARC_MODERN_BIN    a binary you already have
  --release TAG, or ARC_PROOF_KIT_RELEASE
                                   that GitHub release's arc-modern asset, checked
                                   against the release's signed SHA256SUMS
  otherwise                        built from this checkout with cargo (needs rustup
                                   and the MSVC build tools)

Without --submit nothing about this computer is sent anywhere. The only
downloads are the pinned model files from Hugging Face (and, with --release,
the binary from GitHub). Needs curl.exe (built into Windows 10 1803 and later).

.EXAMPLE
powershell -ExecutionPolicy Bypass -File scripts\proof-kit\run.ps1 --dry-run
#>
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$RepoUrl = 'https://github.com/FerrumVir/arc-chain'
# The release-manifest signing key and namespace install.sh trusts.
$ReleaseSigner = 'arc-release namespaces="arc-release-manifest-v1" ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIPs2NAiDRXit9EM96A2GdXZgRqvXtl0lvryEAEAEjQfY arc-release-manifest-v1'
$RepoRoot = (Resolve-Path -LiteralPath (Join-Path $PSScriptRoot '..\..')).Path

function Fail([string]$Message) {
    [Console]::Error.WriteLine("proof kit: $Message")
    exit 1
}

$Bin = $env:ARC_MODERN_BIN
$Release = $env:ARC_PROOF_KIT_RELEASE
$KitDir = $env:ARC_PROOF_KIT_DIR
$KitArgs = New-Object 'System.Collections.Generic.List[string]'
for ($i = 0; $i -lt $args.Count; $i++) {
    $arg = [string]$args[$i]
    if ($arg -eq '--bin' -or $arg -eq '--release' -or $arg -eq '--dir') {
        if ($i + 1 -ge $args.Count) { Fail "$arg needs a value" }
        $value = [string]$args[$i + 1]
        $i++
        if ($arg -eq '--bin') { $Bin = $value }
        elseif ($arg -eq '--release') { $Release = $value }
        else {
            $KitDir = $value
            $KitArgs.Add($arg)
            $KitArgs.Add($value)
        }
    } elseif ($arg -eq '-h' -or $arg -eq '--help') {
        Get-Help -Detailed $PSCommandPath
        exit 0
    } else {
        $KitArgs.Add($arg)
    }
}
if (-not $KitDir) {
    $base = $env:LOCALAPPDATA
    if (-not $base) { $base = Join-Path $env:USERPROFILE 'AppData\Local' }
    $KitDir = Join-Path $base 'arc-proof-kit'
}

function Get-ReleaseBinary([string]$Tag) {
    if ($Tag -notmatch '^v[0-9]+\.[0-9]+\.[0-9]+$') { Fail "--release takes a tag such as v0.8.12, not $Tag" }
    switch ($env:PROCESSOR_ARCHITECTURE) {
        'AMD64' { $asset = 'arc-modern-windows-x86_64.exe' }
        'ARM64' { $asset = 'arc-modern-windows-arm64.exe' }
        default { Fail "no release binary for $($env:PROCESSOR_ARCHITECTURE); build from a checkout instead" }
    }
    $dir = Join-Path $KitDir "bin\$Tag"
    New-Item -ItemType Directory -Force -Path $dir | Out-Null
    $base = "$RepoUrl/releases/download/$Tag"
    function Get-ReleaseFile([string]$Name) {
        $target = Join-Path $dir $Name
        & curl.exe --fail --location --proto '=https' --proto-redir '=https' --tlsv1.2 --retry 3 --silent --show-error --output "$target.download" "$base/$Name"
        if ($LASTEXITCODE -ne 0) { Fail "could not download $base/$Name" }
        Move-Item -LiteralPath "$target.download" -Destination $target -Force
        return $target
    }
    $sums = Get-ReleaseFile 'SHA256SUMS'
    $signature = Get-ReleaseFile 'SHA256SUMS.sig'
    if (-not (Get-Command ssh-keygen.exe -ErrorAction SilentlyContinue)) {
        Fail 'ssh-keygen.exe (the Windows OpenSSH client) is needed to check the release signature'
    }
    $signers = Join-Path $dir 'allowed-signers'
    [System.IO.File]::WriteAllText($signers, "$ReleaseSigner`n")
    $verify = Start-Process -FilePath 'ssh-keygen.exe' -NoNewWindow -Wait -PassThru `
        -RedirectStandardInput $sums -RedirectStandardOutput (Join-Path $dir 'verify.out') `
        -RedirectStandardError (Join-Path $dir 'verify.err') `
        -ArgumentList @('-Y', 'verify', '-f', "`"$signers`"", '-I', 'arc-release', '-n', 'arc-release-manifest-v1', '-s', "`"$signature`"")
    if ($verify.ExitCode -ne 0) { Fail "the SHA256SUMS signature of $Tag is invalid or not from the ARC release key" }
    $expected = $null
    foreach ($line in [System.IO.File]::ReadAllLines($sums)) {
        $parts = $line -split '\s+', 2
        if ($parts.Count -eq 2 -and ($parts[1] -eq $asset -or $parts[1] -eq "*$asset")) { $expected = $parts[0]; break }
    }
    if (-not $expected) { Fail "release $Tag (signature verified) has no $asset; build from a checkout instead" }
    $binary = Get-ReleaseFile $asset
    $actual = (Get-FileHash -LiteralPath $binary -Algorithm SHA256).Hash.ToLowerInvariant()
    if ($actual -ne $expected) { Fail "$asset has SHA-256 $actual; the signed SHA256SUMS says $expected" }
    [Console]::Error.WriteLine("proof kit: using $asset from release $Tag (signature and SHA-256 verified)")
    return $binary
}

function Build-FromCheckout {
    if (-not (Test-Path -LiteralPath (Join-Path $RepoRoot 'crates\arc-inference'))) {
        Fail 'not inside an arc-chain checkout; pass --bin PATH or --release TAG'
    }
    if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
        Fail 'cargo was not found: install Rust from https://rustup.rs with the MSVC build tools (the repository pins its toolchain), or pass --release TAG'
    }
    [Console]::Error.WriteLine('proof kit: building arc-modern from this checkout (the first build takes several minutes)')
    # Release optimisation without fat LTO builds much faster; integer results
    # cannot depend on optimisation settings, which is part of what is tested.
    if (-not $env:CARGO_PROFILE_RELEASE_LTO) { $env:CARGO_PROFILE_RELEASE_LTO = 'off' }
    if (-not $env:CARGO_PROFILE_RELEASE_CODEGEN_UNITS) { $env:CARGO_PROFILE_RELEASE_CODEGEN_UNITS = '16' }
    Push-Location -LiteralPath $RepoRoot
    try {
        # cargo reports progress on stderr and prints nothing on stdout, so
        # stdout stays free for the kit's JSON. (Redirecting a native
        # command's stderr would turn it into errors under Windows PowerShell.)
        & cargo build --release --locked -p arc-inference --bin arc-modern
        if ($LASTEXITCODE -ne 0) { Fail "cargo build failed (exit code $LASTEXITCODE)" }
    } finally {
        Pop-Location
    }
    $target = $env:CARGO_TARGET_DIR
    if (-not $target) { $target = Join-Path $RepoRoot 'target' }
    return (Join-Path $target 'release\arc-modern.exe')
}

if (-not $Bin -and $Release) { $Bin = Get-ReleaseBinary $Release }
if (-not $Bin) { $Bin = Build-FromCheckout }
if (-not (Test-Path -LiteralPath $Bin -PathType Leaf)) { Fail "$Bin is not an arc-modern binary" }

$kit = $KitArgs.ToArray()
& $Bin proof @kit
exit $LASTEXITCODE
