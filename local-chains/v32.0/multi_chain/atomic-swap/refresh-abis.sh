#!/usr/bin/env bash
#
# Vendor the protocol contract ABIs this demo binds against, from a built
# era-contracts checkout, into ./abis/*.json (bare ABI arrays).
#
# The Rust driver's `sol!` bindings read these files at COMPILE time, so the
# contract shapes are sourced from the real compiled artifacts — never
# hand-written. When the contracts change, rebuild era-contracts and re-run
# this script; the driver then fails to compile if a shape it uses changed,
# instead of silently drifting.
#
# Usage:
#   PROTOCOL_CONTRACTS_ROOT=/path/to/era-contracts ./refresh-abis.sh
# (falls back to the atomic-imt-interop checkout next to this repo).

set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ERA="${PROTOCOL_CONTRACTS_ROOT:-$HERE/../../../../../../zksync-era4/contracts}"
OUT="$ERA/l1-contracts/out"

if [[ ! -d "$OUT" ]]; then
  echo "ERROR: era-contracts forge artifacts not found at $OUT" >&2
  echo "  Set PROTOCOL_CONTRACTS_ROOT and build them: (cd l1-contracts && forge build)" >&2
  exit 1
fi

# contract-file : contract-name  (source of truth for the shapes the driver uses)
CONTRACTS=(
  "InteropCenter:InteropCenter"
  "L2InteropHandler:L2InteropHandler"
  "AtomicFlowManager:AtomicFlowManager"
  "L2NativeTokenVault:L2NativeTokenVault"
  "L2InteropRootStorage:L2InteropRootStorage"
  "IERC7786Attributes:IERC7786Attributes"
)

mkdir -p "$HERE/abis"
for entry in "${CONTRACTS[@]}"; do
  file="${entry%%:*}"
  name="${entry##*:}"
  src="$OUT/$file.sol/$name.json"
  dst="$HERE/abis/$name.json"
  if [[ ! -f "$src" ]]; then
    echo "ERROR: missing artifact $src (build era-contracts first)" >&2
    exit 1
  fi
  # Extract just the ABI array so the vendored file is small + diff-stable.
  python3 -c "import json,sys; json.dump(json.load(open('$src'))['abi'], open('$dst','w'), indent=2)"
  echo "vendored abis/$name.json ($(python3 -c "import json;print(len(json.load(open('$dst'))))") entries)"
done

echo "done. era-contracts root: $ERA"
