# ════════════════════════════════════════════════════════════════
#  ARC Network — Join in 60 Seconds (Windows)
#
#  Run AI inference on your device. Earn ARC tokens.
#
#  Usage (PowerShell as Administrator):
#    irm https://raw.githubusercontent.com/FerrumVir/arc-chain/main/scripts/arc-community.ps1 | iex
# ════════════════════════════════════════════════════════════════

$ErrorActionPreference = "Stop"
$ArcDir = "$env:USERPROFILE\.arc"
$RepoUrl = "https://github.com/FerrumVir/arc-chain.git"
$ModelUrl = "https://huggingface.co/TheBloke/Llama-2-7B-Chat-GGUF/resolve/main/llama-2-7b-chat.Q4_K_M.gguf"
$CpuLimit = 15

Write-Host ""
Write-Host "  ╔═══════════════════════════════════════╗" -ForegroundColor White
Write-Host "  ║   ARC Network — Decentralized AI      ║" -ForegroundColor Cyan
Write-Host "  ║   Run inference. Earn tokens.          ║" -ForegroundColor White
Write-Host "  ╚═══════════════════════════════════════╝" -ForegroundColor White
Write-Host ""

# Step 1: Create directories + identity
Write-Host "[1/6] Setting up your node identity" -ForegroundColor Cyan
New-Item -ItemType Directory -Force -Path "$ArcDir\bin", "$ArcDir\data" | Out-Null

$IdentityFile = "$ArcDir\identity.seed"
if (Test-Path $IdentityFile) {
    $Seed = Get-Content $IdentityFile
    Write-Host "  ✓ Identity loaded: $Seed" -ForegroundColor Green
} else {
    $Bytes = New-Object byte[] 8
    [System.Security.Cryptography.RandomNumberGenerator]::Fill($Bytes)
    $Seed = "arc-worker-" + [BitConverter]::ToString($Bytes).Replace("-","").ToLower()
    Set-Content -Path $IdentityFile -Value $Seed
    Write-Host "  ✓ New identity: $Seed" -ForegroundColor Green
}

# Step 2: Get the binary
Write-Host "`n[2/6] Getting the ARC node binary" -ForegroundColor Cyan

$Binary = "$ArcDir\bin\arc-node.exe"
if (Test-Path $Binary) {
    Write-Host "  ✓ Using cached binary" -ForegroundColor Green
} else {
    # Check for Rust
    if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
        Write-Host "  ! Installing Rust..." -ForegroundColor Yellow
        Invoke-WebRequest -Uri "https://win.rustup.rs/x86_64" -OutFile "$env:TEMP\rustup-init.exe"
        Start-Process -Wait -FilePath "$env:TEMP\rustup-init.exe" -ArgumentList "-y", "--default-toolchain", "nightly"
        $env:PATH = "$env:USERPROFILE\.cargo\bin;$env:PATH"
    }
    Write-Host "  ✓ Rust found" -ForegroundColor Green

    $SrcDir = "$ArcDir\src\arc-chain"
    if (Test-Path $SrcDir) {
        Set-Location $SrcDir
        git pull origin main --quiet 2>$null
    } else {
        git clone --depth 1 $RepoUrl $SrcDir 2>$null
        Set-Location $SrcDir
    }

    Write-Host "  Building (first time takes 5-10 minutes)..." -ForegroundColor DarkGray
    cargo build --release -p arc-node
    Copy-Item "target\release\arc-node.exe" $Binary
    Write-Host "  ✓ Build complete" -ForegroundColor Green
}

# Step 3: Download model
Write-Host "`n[3/6] Downloading AI model (Llama 2 7B, 4.1 GB)" -ForegroundColor Cyan
$ModelPath = "$ArcDir\model.gguf"
if (Test-Path $ModelPath) {
    Write-Host "  ✓ Model already downloaded" -ForegroundColor Green
} else {
    Write-Host "  Downloading..." -ForegroundColor DarkGray
    Invoke-WebRequest -Uri $ModelUrl -OutFile "$ModelPath.tmp" -UseBasicParsing
    Move-Item "$ModelPath.tmp" $ModelPath
    Write-Host "  ✓ Model downloaded" -ForegroundColor Green
}

# Step 4: Configure
Write-Host "`n[4/6] Configuring network" -ForegroundColor Cyan
$SeedsContent = @"
149.28.32.76:9091
140.82.16.112:9091
136.244.109.1:9091
104.238.171.11:9091
202.182.107.41:9091
149.28.153.31:9091
216.238.120.27:9091
139.84.237.49:9091
"@
Set-Content -Path "$ArcDir\seeds.txt" -Value $SeedsContent
Write-Host "  ✓ Network configured (8 seed validators)" -ForegroundColor Green

# Step 5: Start node
Write-Host "`n[5/6] Starting inference node" -ForegroundColor Cyan

# Stop existing
$OldPid = Get-Content "$ArcDir\node.pid" -ErrorAction SilentlyContinue
if ($OldPid) { Stop-Process -Id $OldPid -Force -ErrorAction SilentlyContinue; Start-Sleep 2 }

$GenesisPath = "$ArcDir\src\arc-chain\genesis.toml"
$Args = "--rpc 0.0.0.0:9090 --seeds-file `"$ArcDir\seeds.txt`" --genesis `"$GenesisPath`" --validator-seed $Seed --stake 5000000 --mode worker --cpu-limit $CpuLimit --eth-rpc-port 0 --model `"$ModelPath`""

$Process = Start-Process -FilePath $Binary -ArgumentList $Args -WorkingDirectory (Split-Path $GenesisPath) -WindowStyle Hidden -PassThru -RedirectStandardOutput "$ArcDir\node.log" -RedirectStandardError "$ArcDir\node-err.log"
Set-Content -Path "$ArcDir\node.pid" -Value $Process.Id
Write-Host "  ✓ Node started (PID: $($Process.Id))" -ForegroundColor Green

Start-Sleep 10
try {
    $Health = Invoke-RestMethod -Uri "http://localhost:9090/health" -TimeoutSec 5
    Write-Host "  ✓ Connected to $($Health.peers) peers" -ForegroundColor Green
} catch {
    Write-Host "  ! Still connecting..." -ForegroundColor Yellow
}

# Step 6: Auto-start + open dashboard
Write-Host "`n[6/6] Setting up auto-start" -ForegroundColor Cyan

$TaskName = "ARC Inference Node"
$TaskAction = New-ScheduledTaskAction -Execute $Binary -Argument $Args -WorkingDirectory (Split-Path $GenesisPath)
$TaskTrigger = New-ScheduledTaskTrigger -AtLogOn
$TaskSettings = New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries -StartWhenAvailable
try {
    Register-ScheduledTask -TaskName $TaskName -Action $TaskAction -Trigger $TaskTrigger -Settings $TaskSettings -Force | Out-Null
    Write-Host "  ✓ Auto-start configured (Task Scheduler)" -ForegroundColor Green
} catch {
    Write-Host "  ! Could not set auto-start (run as Admin to enable)" -ForegroundColor Yellow
}

Start-Process "http://localhost:9090/worker/dashboard"

Write-Host ""
Write-Host "  ════════════════════════════════════════" -ForegroundColor White
Write-Host "  Your ARC node is running!" -ForegroundColor Green
Write-Host "  ════════════════════════════════════════" -ForegroundColor White
Write-Host ""
Write-Host "  Dashboard:  http://localhost:9090/worker/dashboard" -ForegroundColor Blue
Write-Host "  Logs:       Get-Content $ArcDir\node.log -Tail 20" -ForegroundColor Blue
Write-Host "  Stop:       Stop-Process -Id $(Get-Content $ArcDir\node.pid)" -ForegroundColor Yellow
Write-Host ""
