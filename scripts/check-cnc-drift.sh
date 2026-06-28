#!/usr/bin/env bash
# check-cnc-drift.sh — report any divergence between the repo's crate SOURCES and
# the cnc build trees. Green = the trees are a faithful projection of the repo and
# nobody has hand-patched them. Run from kokonoe (Git Bash). Exits non-zero on drift.
#
# Compares only src/*.rs (ignores .bak files, Cargo.lock, target/). If it reports
# drift, reconcile with: scripts/deploy-cnc.sh --sync-only (repo wins), OR — if the
# tree has a change the repo lacks — pull that change INTO the repo first.
set -uo pipefail
REPO=${REPO:-/j/nightdrive}
CNC=${CNC:-cnc-server}
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT
drift=0
for tree in /opt/nightdrive-ws /opt/nightdrive/src; do
  tag=$(echo "$tree" | tr '/' '_')
  scp -q -r "$CNC:$tree/crates" "$TMP/$tag" 2>/dev/null || { echo "WARN: cannot read $tree/crates"; continue; }
  for crate in $(ls "$TMP/$tag"); do
    crate=${crate%/}
    [ -d "$REPO/crates/$crate/src" ] || continue
    d=$(diff -rq "$TMP/$tag/$crate/src" "$REPO/crates/$crate/src" 2>/dev/null | grep -vE '\.bak')
    if [ -n "$d" ]; then echo "DRIFT [$tree] $crate:"; echo "$d" | sed 's/^/   /'; drift=1; fi
  done
done
echo
if [ "$drift" = 0 ]; then
  echo "OK — cnc trees match repo (no source drift)"
else
  echo "DRIFT DETECTED — reconcile before deploying (see header)."; exit 1
fi
