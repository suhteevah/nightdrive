# install_acestep.ps1 - install + smoke-test the ACE-Step 1.5 sidecar locally.
#
# ACE-Step 1.5 ships with `uv` as its package manager and expects a fresh
# venv. Don't try to install it into the synthwave-gen venv -- torch/diffusers
# versions conflict. This script:
#   1. Installs `uv` if missing
#   2. Clones the ACE-Step-1.5 repo to J:\acestep
#   3. Runs `uv sync` to build the venv
#   4. Downloads the ACE-Step v1.5 weights (~10 GB, first run)
#   5. Smoke-tests with a handler init
#
# Re-run idempotent: if J:\acestep already exists, `git pull` + `uv sync`.
#
# Run via:
#   powershell -ExecutionPolicy Bypass -File scripts/install_acestep.ps1
#
# Optional env vars (set before running if you want overrides):
#   $env:NIGHTDRIVE_ACESTEP_ROOT   = "J:\acestep"
#   $env:NIGHTDRIVE_ACESTEP_REPO   = "https://github.com/ace-step/ACE-Step-1.5"
#   $env:NIGHTDRIVE_ACESTEP_CONFIG = "acestep-v15-turbo"

$ErrorActionPreference = "Stop"
$ProgressPreference = "SilentlyContinue"

function Write-Step($msg) { Write-Host "`n=== $msg ===" -ForegroundColor Cyan }
function Write-Info($msg) { Write-Host "  $msg" -ForegroundColor Gray }
function Write-Ok($msg)   { Write-Host "  OK: $msg" -ForegroundColor Green }
function Write-Warn($msg) { Write-Host "  WARN: $msg" -ForegroundColor Yellow }
function Write-Err($msg)  { Write-Host "  ERROR: $msg" -ForegroundColor Red }

$AceStepRoot = if ($env:NIGHTDRIVE_ACESTEP_ROOT)   { $env:NIGHTDRIVE_ACESTEP_ROOT }   else { "J:\acestep" }
$AceStepRepo = if ($env:NIGHTDRIVE_ACESTEP_REPO)   { $env:NIGHTDRIVE_ACESTEP_REPO }   else { "https://github.com/ace-step/ACE-Step-1.5" }
$AceStepConfig = if ($env:NIGHTDRIVE_ACESTEP_CONFIG) { $env:NIGHTDRIVE_ACESTEP_CONFIG } else { "acestep-v15-turbo" }

Write-Step "ACE-Step 1.5 install plan"
Write-Info "Install location: $AceStepRoot"
Write-Info "Source repo:      $AceStepRepo"
Write-Info "Model config:     $AceStepConfig"
Write-Info "Expected disk:    ~10 GB for weights + ~2 GB for venv"
Write-Info "Expected time:    ~10-20 min on a decent connection"

# 1. uv installer
Write-Step "[1/5] Verifying uv is installed"
$uv = Get-Command uv -ErrorAction SilentlyContinue
if (-not $uv) {
    Write-Info "uv not found; installing via the official installer"
    try {
        Invoke-RestMethod https://astral.sh/uv/install.ps1 | Invoke-Expression
        $env:Path = "$env:USERPROFILE\.local\bin;$env:Path"
        $uv = Get-Command uv -ErrorAction Stop
    }
    catch {
        Write-Err "uv install failed: $_"
        Write-Err "Manual: https://docs.astral.sh/uv/getting-started/installation/"
        exit 1
    }
}
Write-Ok ("uv at " + $uv.Source)
& uv --version

# 2. git clone (or pull)
Write-Step "[2/5] Cloning ACE-Step-1.5"
if (Test-Path $AceStepRoot) {
    Write-Info "Directory exists -- running git pull"
    Push-Location $AceStepRoot
    try {
        git pull --ff-only
    }
    catch {
        Write-Warn "git pull failed (continuing): $_"
    }
    Pop-Location
}
else {
    $parent = Split-Path -Parent $AceStepRoot
    if (-not (Test-Path $parent)) {
        New-Item -ItemType Directory -Path $parent -Force | Out-Null
    }
    git clone $AceStepRepo $AceStepRoot
    if ($LASTEXITCODE -ne 0) {
        Write-Err "git clone failed (exit $LASTEXITCODE)"
        exit 1
    }
}
Write-Ok "repo at $AceStepRoot"

# 3. uv sync -- installs the project's pinned deps into .venv
Write-Step "[3/5] Running uv sync (installs torch/diffusers/etc into ACE-Step's .venv)"
Push-Location $AceStepRoot
try {
    # uv reads pyproject.toml + uv.lock here. Pinned versions for the 1.5
    # release; do not pass --upgrade.
    & uv sync
    if ($LASTEXITCODE -ne 0) { throw "uv sync exit $LASTEXITCODE" }
}
catch {
    Write-Err "uv sync failed: $_"
    Write-Err "Common causes:"
    Write-Err "  - Python 3.11 or 3.12 not on PATH (uv will install one if missing)"
    Write-Err "  - CUDA-toolkit mismatch -- ACE-Step 1.5 pins torch with cu128 wheels"
    Write-Err "  - Network: HF/PyPI rate-limit"
    Pop-Location
    exit 1
}
Pop-Location
Write-Ok "uv sync complete"

# 4. Pre-download model weights. ACE-Step pulls them from HF on first
#    handler init; doing it now keeps progress visible to the operator.
Write-Step "[4/5] Pre-downloading model weights (the slow step, ~10 GB)"
Push-Location $AceStepRoot
try {
    $smoke = @'
import sys, os, time
os.environ.setdefault("HF_HUB_DISABLE_PROGRESS_BARS", "0")
t0 = time.time()
print("[smoke] importing acestep...", flush=True)
try:
    from acestep.handler import AceStepHandler
    print(f"[smoke] AceStepHandler imported in {time.time()-t0:.1f}s", flush=True)
except Exception as e:
    print(f"[smoke] FAIL: import error: {e}", flush=True)
    sys.exit(1)
t1 = time.time()
print("[smoke] initializing handler (downloads weights on first run)...", flush=True)
config_path = os.environ.get("NIGHTDRIVE_ACESTEP_CONFIG", "acestep-v15-turbo")
try:
    h = AceStepHandler()
    h.initialize_service(project_root=os.getcwd(), config_path=config_path, device="cuda:0")
    print(f"[smoke] handler init OK in {time.time()-t1:.1f}s", flush=True)
except Exception as e:
    print(f"[smoke] handler init FAILED: {e}", flush=True)
    sys.exit(2)
print("[smoke] DONE", flush=True)
'@
    $smokePath = Join-Path $AceStepRoot "_nd_smoke.py"
    $smoke | Out-File -FilePath $smokePath -Encoding utf8 -Force
    $env:NIGHTDRIVE_ACESTEP_CONFIG = $AceStepConfig
    & uv run python $smokePath
    $smokeExit = $LASTEXITCODE
    Remove-Item $smokePath -ErrorAction SilentlyContinue
    if ($smokeExit -ne 0) {
        Write-Warn "smoke import-and-init failed (exit $smokeExit). The sidecar will surface the same error on startup."
    } else {
        Write-Ok "model weights present, handler initialized clean"
    }
}
finally {
    Pop-Location
}

# 5. Show how to run the sidecar
Write-Step "[5/5] Sidecar run command"
Write-Info "To start the ACE-Step sidecar:"
Write-Host ""
Write-Host "  cd J:\nightdrive" -ForegroundColor White
Write-Host "  `$env:NIGHTDRIVE_ACESTEP_ROOT = `"$AceStepRoot`"" -ForegroundColor White
Write-Host "  `$env:NIGHTDRIVE_ACESTEP_CONFIG = `"$AceStepConfig`"" -ForegroundColor White
Write-Host "  & `"$AceStepRoot\.venv\Scripts\python.exe`" -m uvicorn sidecar.acestep_server:app --host 127.0.0.1 --port 8083 --workers 1" -ForegroundColor White
Write-Host ""
Write-Info "Then verify with:"
Write-Host "  curl http://127.0.0.1:8083/health" -ForegroundColor White
Write-Host ""
Write-Info "On cnc P100s (sm_60), additionally set:"
Write-Host "  `$env:ACESTEP_LM_BACKEND = `"pt`"" -ForegroundColor White
Write-Info "(forces PyTorch backend instead of vLLM; required for Pascal)"
Write-Host ""
Write-Ok "install playbook complete"
