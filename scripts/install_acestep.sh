#!/usr/bin/env bash
# install_acestep.sh - linux port of install_acestep.ps1.
#
# Mirrors the kokonoe Windows install at J:\acestep on a linux host (intended:
# cnc-server, openSUSE Leap Micro 6.2, 2-3x Tesla P100). Same 5-step structure,
# same idempotency, same env-var overrides. No host packages are installed by
# this script -- ACE-Step's deps go inside `uv sync` and ffmpeg is not needed
# on the inference box.
#
# Run via:
#   bash scripts/install_acestep.sh
#
# Optional env vars:
#   NIGHTDRIVE_ACESTEP_ROOT    install location (default /opt/acestep)
#   NIGHTDRIVE_ACESTEP_REPO    git URL (default ACE-Step-1.5 upstream)
#   NIGHTDRIVE_ACESTEP_CONFIG  weight bundle  (default acestep-v15-turbo)
#
# Pascal P100 (sm_60) caveat: ACE-Step's vLLM backend wants sm_70+. The sidecar
# detects sm_60 at runtime and falls back to the PyTorch backend automatically,
# but you can short-circuit the check by exporting ACESTEP_LM_BACKEND=pt before
# launching the sidecar (done in the systemd unit -- see
# scripts/nightdrive-acestep.service).

set -euo pipefail

ACESTEP_ROOT="${NIGHTDRIVE_ACESTEP_ROOT:-/opt/acestep}"
ACESTEP_REPO="${NIGHTDRIVE_ACESTEP_REPO:-https://github.com/ace-step/ACE-Step-1.5}"
ACESTEP_CFG="${NIGHTDRIVE_ACESTEP_CONFIG:-acestep-v15-turbo}"

c_cyan=$'\033[36m'; c_gray=$'\033[90m'; c_grn=$'\033[32m'; c_yel=$'\033[33m'; c_red=$'\033[31m'; c_rst=$'\033[0m'
step() { printf "\n%s=== %s ===%s\n" "$c_cyan" "$1" "$c_rst"; }
info() { printf "  %s%s%s\n" "$c_gray" "$1" "$c_rst"; }
ok()   { printf "  %sOK: %s%s\n" "$c_grn"  "$1" "$c_rst"; }
warn() { printf "  %sWARN: %s%s\n" "$c_yel" "$1" "$c_rst"; }
err()  { printf "  %sERROR: %s%s\n" "$c_red" "$1" "$c_rst"; }

step "ACE-Step 1.5 install plan"
info "Install location: $ACESTEP_ROOT"
info "Source repo:      $ACESTEP_REPO"
info "Model config:     $ACESTEP_CFG"
info "Expected disk:    ~10 GB weights + ~2 GB venv"
info "Expected time:    ~10-20 min on a decent connection"

# 1. uv installer
step "[1/5] Verifying uv is installed"
if ! command -v uv >/dev/null 2>&1; then
    info "uv not found; installing via the official installer"
    if ! curl -LsSf https://astral.sh/uv/install.sh | sh; then
        err "uv install failed"
        err "Manual: https://docs.astral.sh/uv/getting-started/installation/"
        exit 1
    fi
    export PATH="$HOME/.local/bin:$PATH"
fi
ok "uv at $(command -v uv)"
uv --version

# 2. git clone (or pull)
step "[2/5] Cloning ACE-Step-1.5"
if [[ -d "$ACESTEP_ROOT/.git" ]]; then
    info "Directory exists -- running git pull"
    (cd "$ACESTEP_ROOT" && git pull --ff-only || warn "git pull failed (continuing)")
else
    parent="$(dirname "$ACESTEP_ROOT")"
    [[ -d "$parent" ]] || mkdir -p "$parent"
    git clone "$ACESTEP_REPO" "$ACESTEP_ROOT"
fi
ok "repo at $ACESTEP_ROOT"

# 3. uv sync
step "[3/5] Running uv sync (installs torch/diffusers/etc into ACE-Step's .venv)"
(
    cd "$ACESTEP_ROOT"
    # uv reads pyproject.toml + uv.lock. Do NOT pass --upgrade -- the 1.5
    # release pins exact torch/diffusers versions.
    uv sync
)
ok "uv sync complete"

# 4. Pre-download model weights via a handler-init smoke. Same Python smoke
#    used in the PS1; failure here surfaces the same error you'd see on first
#    sidecar boot -- better to know now.
step "[4/5] Pre-downloading model weights (the slow step, ~10 GB)"
SMOKE="$(mktemp -p "$ACESTEP_ROOT" _nd_smoke.XXXXXX.py)"
trap 'rm -f "$SMOKE"' EXIT
cat > "$SMOKE" <<'PY'
import os, sys, time
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
PY
(
    cd "$ACESTEP_ROOT"
    export NIGHTDRIVE_ACESTEP_CONFIG="$ACESTEP_CFG"
    # Pascal sm_60: skip the vLLM detect path early.
    export ACESTEP_LM_BACKEND="${ACESTEP_LM_BACKEND:-pt}"
    if uv run python "$SMOKE"; then
        ok "model weights present, handler initialized clean"
    else
        warn "smoke import-and-init failed -- the sidecar will surface the same error on startup"
    fi
)

# 4b. Sidecar web deps -- FastAPI / uvicorn / pydantic aren't in ACE-Step's
#     pyproject; the nightdrive sidecar layer needs them. Idempotent.
step "[4b] Installing sidecar web deps (fastapi + uvicorn + pydantic + soundfile)"
(
    cd "$ACESTEP_ROOT"
    uv pip install --quiet "fastapi>=0.110" "uvicorn[standard]>=0.27" "pydantic>=2.5" "soundfile>=0.12"
)
ok "sidecar web deps installed"

# 5. Show how to run the sidecar
step "[5/5] Sidecar run command"
info "Manual launch (one-shot, foreground):"
cat <<EOM

  cd /opt/nightdrive
  NIGHTDRIVE_ACESTEP_ROOT="$ACESTEP_ROOT" \\
  NIGHTDRIVE_ACESTEP_CONFIG="$ACESTEP_CFG" \\
  ACESTEP_LM_BACKEND=pt \\
  CUDA_VISIBLE_DEVICES=1 \\
  "$ACESTEP_ROOT/.venv/bin/python" -m uvicorn sidecar.acestep_server:app \\
      --host 0.0.0.0 --port 8083 --workers 1

  # then verify:
  curl -s http://127.0.0.1:8083/health | jq .

EOM
info "Or install the systemd unit:"
cat <<EOM

  sudo install -m 0644 scripts/nightdrive-acestep.service /etc/systemd/system/
  sudo systemctl daemon-reload
  sudo systemctl enable --now nightdrive-acestep.service
  systemctl status nightdrive-acestep
  journalctl -u nightdrive-acestep -f

EOM
info "CUDA_VISIBLE_DEVICES=1 pins ACE-Step to GPU 1 (the 16 GB card)."
info "GPU 0 (12 GB) stays free for SDXL / fanout / experiments."
ok "install playbook complete"
