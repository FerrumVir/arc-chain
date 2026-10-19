<#
.SYNOPSIS
Run ARC's cross-platform determinism proof on Windows and print the combined
SHA-256 of its transcript.

.DESCRIPTION
The pinned Llama-2-7B-Chat Q4_K_M weights, the fixed prompts in prompts.json
and greedy decoding produce a byte-identical transcript on every CPU platform
the CI proof covers. Compare the printed hash with the published value in
docs/determinism-proof.md (also in expected-sha256.txt). CPU only.

Needs rustup with the MSVC build tools (the repository pins its toolchain),
curl.exe (built into Windows 10 1803 and later), about 5 GB of disk for the
model and about 8 GB of free memory; less memory works, slowly, through the
page file. docs/determinism-proof.md lists measured CI run times.

.EXAMPLE
powershell -ExecutionPolicy Bypass -File scripts\determinism-proof\run-proof.ps1 -Kernel scalar
#>
[CmdletBinding()]
param(
    [ValidateSet('scalar', 'simd')]
    [string]$Kernel = 'scalar',
    [string]$Model = $env:ARC_PROOF_MODEL,
    [string]$OutDir = $env:ARC_PROOF_OUT_DIR,
    [ValidateRange(1, 4000)]
    [int]$MaxNewTokens = 32,
    [ValidateRange(0, 1000)]
    [int]$PromptLimit = 0,
    [string]$Shard = '',
    [ValidateRange(0, 604800)]
    [int]$DeadlineSeconds = 0
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$ModelUrl = 'https://huggingface.co/TheBloke/Llama-2-7B-Chat-GGUF/resolve/191239b3e26b2882fb562ffccdd1cf0f65402adb/llama-2-7b-chat.Q4_K_M.gguf'
$ModelSha256 = '08a5566d61d7cb6b420c3e4387a39e0078e1f2fe5f055f3a03887385304d4bfa'
$ModelBytes = 4081004224
$DefaultMaxNewTokens = 32

$ScriptDir = $PSScriptRoot
$RepoRoot = (Resolve-Path -LiteralPath (Join-Path $ScriptDir '..\..')).Path

if (-not $Model) {
    $modelDir = $env:ARC_PROOF_MODEL_DIR
    if (-not $modelDir) { $modelDir = Join-Path $env:LOCALAPPDATA 'arc-determinism-proof' }
    $Model = Join-Path $modelDir 'llama-2-7b-chat.Q4_K_M.gguf'
}
if (-not $OutDir) { $OutDir = Join-Path $RepoRoot 'target\determinism-proof' }

if (-not (Test-Path -LiteralPath $Model -PathType Leaf)) {
    New-Item -ItemType Directory -Force -Path (Split-Path -Parent $Model) | Out-Null
    Write-Host "Downloading the pinned model (4.1 GB) to $Model"
    & curl.exe --fail --location --retry 5 --connect-timeout 20 --proto '=https' --tlsv1.2 --output "$Model.partial" $ModelUrl
    if ($LASTEXITCODE -ne 0) { throw "model download failed (curl exit code $LASTEXITCODE)" }
    Move-Item -LiteralPath "$Model.partial" -Destination $Model -Force
}

$size = (Get-Item -LiteralPath $Model).Length
if ($size -ne $ModelBytes) {
    throw "model is $size bytes, expected $ModelBytes (wrong or truncated file at $Model)"
}
Write-Host "Verifying the model's SHA-256 ..."
$actualSha256 = (Get-FileHash -LiteralPath $Model -Algorithm SHA256).Hash.ToLowerInvariant()
if ($actualSha256 -ne $ModelSha256) {
    throw "model SHA-256 is $actualSha256, expected $ModelSha256"
}
Write-Host "model OK: sha256 $actualSha256"

New-Item -ItemType Directory -Force -Path $OutDir | Out-Null
$suffix = $Kernel
if ($Shard) { $suffix = "$Kernel-shard-" + ($Shard -replace '/', '-of-') }
$transcript = Join-Path $OutDir "transcript-$suffix.txt"
$runJson = Join-Path $OutDir "run-$suffix.json"
$cargoArgs = @(
    'run', '--locked', '--release', '-p', 'arc-inference', '--features', 'candle',
    '--example', 'determinism_proof', '--',
    '--model', $Model,
    '--prompts', (Join-Path $ScriptDir 'prompts.json'),
    '--kernel', $Kernel,
    '--max-new-tokens', "$MaxNewTokens",
    '--transcript', $transcript,
    '--run-json', $runJson
)
if ($PromptLimit -gt 0) { $cargoArgs += @('--prompt-limit', "$PromptLimit") }
if ($Shard) { $cargoArgs += @('--shard', $Shard) }
if ($DeadlineSeconds -gt 0) { $cargoArgs += @('--deadline-seconds', "$DeadlineSeconds") }

$exitCode = 1
Push-Location -LiteralPath $RepoRoot
try {
    & cargo @cargoArgs
    $exitCode = $LASTEXITCODE
} finally {
    Pop-Location
}
if ($exitCode -ne 0) { throw "determinism_proof exited with code $exitCode" }

$combined = (Get-FileHash -LiteralPath $transcript -Algorithm SHA256).Hash.ToLowerInvariant()
Write-Host ''
Write-Host "transcript:        $transcript"
Write-Host "combined SHA-256:  $combined"
$expectedFile = Join-Path $ScriptDir 'expected-sha256.txt'
if ($PromptLimit -gt 0 -or $Shard -or $MaxNewTokens -ne $DefaultMaxNewTokens) {
    Write-Host 'Non-default workload: compare with another machine that used the same options.'
} elseif (Test-Path -LiteralPath $expectedFile -PathType Leaf) {
    $expected = Get-Content -LiteralPath $expectedFile |
        Where-Object { $_ -match '^[0-9a-f]{64}$' } |
        Select-Object -First 1
    if (-not $expected) { $expected = 'none recorded' }
    Write-Host "published SHA-256: $expected"
    if ($combined -eq $expected) {
        Write-Host 'RESULT: MATCH. This machine produced the published transcript byte for byte.'
    } else {
        Write-Host "RESULT: DIFFERENT. Please report it with $runJson and $transcript attached."
        exit 3
    }
}
