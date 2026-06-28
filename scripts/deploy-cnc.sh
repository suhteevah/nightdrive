#!/usr/bin/env bash
# deploy-cnc.sh — THE sanctioned path to deploy nightdrive code from the repo to
# cnc. The repo (J:\nightdrive) is the SINGLE SOURCE OF TRUTH; this mirrors its
# crate SOURCES into cnc's two trimmed build trees, then (unless --sync-only)
# rebuilds + installs the two binaries. NEVER hand-edit the cnc trees — edit the
# repo and run this. Run from kokonoe (Git Bash).
#
#   scripts/deploy-cnc.sh              # sync sources + rebuild + install both binaries
#   scripts/deploy-cnc.sh --sync-only  # only mirror sources (no rebuild) — safe anytime
#   scripts/deploy-cnc.sh cli          # restrict to the cli  tree (/opt/nightdrive-ws)
#   scripts/deploy-cnc.sh orch         # restrict to the orch tree (/opt/nightdrive/src)
#
# WHY two trees: neither cnc tree holds the full 15-crate workspace. /opt/nightdrive-ws
# (7 crates incl album-composer + openclaw-main) builds nightdrive-cli; /opt/nightdrive/src
# (11 crates incl encoder + audio-gen) builds nightdrive-orchestrator. Each tree is a
# trimmed workspace with TREE-LOCAL manifests: the workspace-root Cargo.toml lists only
# its subset of members, and some per-crate Cargo.toml are trimmed too (e.g. the src tree
# dropped nightdrive-cli entirely — cli path-deps album-composer/openclaw-main which that
# tree doesn't carry). So we mirror per-crate src/ SOURCE ONLY — never any Cargo.toml.
# If a repo change adds a real dependency, hand-edit that tree's Cargo.toml (rare; the
# build will say so). Source (*.rs) is identical across repo and trees; manifests are infra.
set -euo pipefail
REPO=${REPO:-/j/nightdrive}
CNC=${CNC:-cnc-server}
STAMP=$(date +%Y%m%d-%H%M%S)
SYNC_ONLY=0; ONLY=""
for a in "$@"; do case "$a" in
  --sync-only) SYNC_ONLY=1;;
  cli) ONLY=cli;;
  orch) ONLY=orch;;
  *) echo "unknown arg: $a (use --sync-only | cli | orch)"; exit 2;;
esac; done

sync_tree() {  # $1=remote tree path
  local tree=$1
  echo "[deploy] mirror repo crates -> $tree"
  for crate in $(ssh "$CNC" "ls $tree/crates"); do
    crate=${crate%/}
    if [ ! -d "$REPO/crates/$crate/src" ]; then echo "   skip $crate (not in repo)"; continue; fi
    ssh "$CNC" "rm -rf $tree/crates/$crate/src"
    scp -q -r "$REPO/crates/$crate/src" "$CNC:$tree/crates/$crate/"
    echo "   synced $crate (src only)"
  done
}

build_install() {  # $1=tree  $2=target crate  $3=bin name
  local tree=$1 target=$2 bin=$3
  echo "[deploy] build $bin in $tree"
  ssh "$CNC" "export PATH=\$PATH:/root/.cargo/bin; cd $tree && cargo build --release -p $target"
  echo "[deploy] install $bin (backup -> $bin.bak-$STAMP)"
  ssh "$CNC" "cp /opt/nightdrive/bin/$bin /opt/nightdrive/bin/$bin.bak-$STAMP && cp $tree/target/release/$bin /opt/nightdrive/bin/$bin && /opt/nightdrive/bin/$bin --help >/dev/null && echo '   $bin OK'"
}

if [ -z "$ONLY" ] || [ "$ONLY" = cli ]; then
  sync_tree /opt/nightdrive-ws
  [ "$SYNC_ONLY" = 0 ] && build_install /opt/nightdrive-ws nightdrive-cli nightdrive-cli
fi
if [ -z "$ONLY" ] || [ "$ONLY" = orch ]; then
  sync_tree /opt/nightdrive/src
  [ "$SYNC_ONLY" = 0 ] && build_install /opt/nightdrive/src nightdrive-orchestrator nightdrive-orchestrator
fi
echo "[deploy] DONE ($STAMP)$([ "$SYNC_ONLY" = 1 ] && echo ' — sync-only; binaries unchanged')"
