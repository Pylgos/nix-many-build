#!/usr/bin/env bash

pkgs=(pkg_{a..h})

cleanup() {
  for pkg in ${pkgs[@]}; do
    echo "Cleaning up $pkg"
    local outPath
    outPath=$(nix eval .#test.x86_64-linux.$pkg.outPath --json | jq -r . 2>/dev/null)
    if [[ -d $outPath ]]; then
      echo "Deleting $outPath"
      nix store delete "$outPath"
    fi
  done
}

should_success() {
  local msg=$1
  shift 1
  if ! "$@"; then
    echo "should_success: $msg: $@" >&2
    exit 1
  fi
}

should_fail() {
  local msg=$1
  shift 1
  if "$@"; then
    echo "should_fail: $msg: $@" >&2
    exit 1
  fi
}

cleanup

should_success "--keep-going" cargo run -- --keep-going .#test.x86_64-linux
cleanup

should_fail "propagate build error" cargo run -- .#test.x86_64-linux
cleanup

