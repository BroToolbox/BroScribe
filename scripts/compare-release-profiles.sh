#!/usr/bin/env bash
# Measure candidate release profiles before choosing one (Cargo.toml defines
# release, release-thin-lto and release-cgu1). It records, per profile, the
# clean build time and executable sizes, and optionally times a command you
# supply against each profile's executables. It changes nothing in the
# repository and decides nothing: adopt a setting in [profile.release] only
# from results recorded on the target hardware.
#
#   scripts/compare-release-profiles.sh [--profiles "release release-thin-lto"]
#       [--runs N] [-- command args…]
#
# The optional command runs with BROSCRIBE_PROFILE_BIN pointing at the
# profile's output folder, e.g. a synthetic benchmark:
#   scripts/compare-release-profiles.sh -- sh -c '"$BROSCRIBE_PROFILE_BIN/broscribe" bench …'
# Results: target/profile-comparison/results.tsv (one row per profile/run).
set -euo pipefail

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo"
# shellcheck source=common.sh
source "$repo/scripts/common.sh"

profiles="release release-thin-lto release-cgu1"
runs=3
command=()
while [[ $# -gt 0 ]]; do
    case "$1" in
        --profiles) profiles="${2:?--profiles needs a list}"; shift 2 ;;
        --runs) runs="${2:?--runs needs a number}"; shift 2 ;;
        --) shift; command=("$@"); break ;;
        *) echo "usage: $0 [--profiles LIST] [--runs N] [-- command args…]" >&2; exit 2 ;;
    esac
done
if [[ ! "$runs" =~ ^[1-9][0-9]*$ ]]; then echo "--runs must be a positive integer" >&2; exit 2; fi

require_cargo_1_100
out_root="${CARGO_TARGET_DIR:-$repo/target}/profile-comparison"
mkdir -p "$out_root"
results="$out_root/results.tsv"
printf 'profile\tmetric\trun\tvalue\n' >"$results"
binaries=(broscribe-app broscribe broscribed broscribe-worker broscribe-update-runner)

now() { python3 -c 'import time; print(f"{time.monotonic():.3f}")'; }
elapsed() { python3 -c 'import sys; print(f"{float(sys.argv[2]) - float(sys.argv[1]):.3f}")' "$1" "$2"; }

for profile in $profiles; do
    target_dir="$out_root/$profile"
    rm -rf "$target_dir"
    start="$(now)"
    CARGO_TARGET_DIR="$target_dir" cargo build --locked --profile "$profile" \
        -p broscribe-app -p broscribe-cli -p broscribed -p broscribe-worker -p broscribe-update-runner
    printf '%s\tclean_build_seconds\t1\t%s\n' "$profile" "$(elapsed "$start" "$(now)")" >>"$results"
    bin_dir="$target_dir/$profile"
    for binary in "${binaries[@]}"; do
        printf '%s\tbytes:%s\t1\t%s\n' "$profile" "$binary" "$(wc -c <"$bin_dir/$binary" | tr -d ' ')" >>"$results"
    done
    if [[ ${#command[@]} -gt 0 ]]; then
        for run in $(seq 1 "$runs"); do
            start="$(now)"
            BROSCRIBE_PROFILE_BIN="$bin_dir" "${command[@]}" >/dev/null
            printf '%s\tcommand_seconds\t%s\t%s\n' "$profile" "$run" "$(elapsed "$start" "$(now)")" >>"$results"
        done
    fi
done
echo "results: $results"
