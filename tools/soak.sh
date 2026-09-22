#!/usr/bin/env bash
# Build an instrumented binary, then run the bounded load/fault harness.
set -Eeuo pipefail
script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
cd "$script_dir/.."
profile=release
build=true
args=()
for arg in "$@"; do
  case "$arg" in
    --debug) profile=debug ;;
    --release) profile=release ;;
    --no-build) build=false ;;
    --help|-h)
      echo 'Wrapper options: --debug, --release (default), --no-build. RUSHLS_BIN overrides the binary.'
      exec python3 "$script_dir/soak.py" --help ;;
    *) args+=("$arg") ;;
  esac
done
if [[ "$build" == true ]]; then
  build_args=()
  [[ "$profile" == release ]] && build_args+=(--release)
  cargo build -p rushls --features allocation-counting --bin rushls "${build_args[@]}"
fi
target_dir="$(cargo metadata --no-deps --format-version 1 | python3 -c 'import json,sys; print(json.load(sys.stdin)["target_directory"])')"
exec python3 "$script_dir/soak.py" --binary "${RUSHLS_BIN:-$target_dir/$profile/rushls}" "${args[@]}"
