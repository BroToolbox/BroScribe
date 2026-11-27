# Helpers sourced by the build scripts; never executed directly.

# Fails unless the active Cargo is at least 1.100, the first release that
# enforces registry.global-min-publish-age (AGENTS.md, "Dependencies").
require_cargo_1_100() {
  local version major minor
  version="$(cargo --version | awk '{ print $2 }')"
  IFS=. read -r major minor _ <<<"${version%%-*}"
  if [[ ! "$major" =~ ^[0-9]+$ || ! "$minor" =~ ^[0-9]+$ ]] \
    || ((major < 1 || (major == 1 && minor < 100))); then
    echo "Cargo ${version} does not enforce the three-day publish-age rule; use the pinned 1.100.0 toolchain (rust-toolchain.toml)" >&2
    return 1
  fi
}

# Prints `true` when the git working tree at $1 differs from its HEAD commit: a
# tracked change, staged or not, or an untracked file .gitignore does not
# exclude (a new source file is built but is not in the commit), else `false`.
# Fails when git cannot read the tree's status, so a build is never recorded as
# clean by default. Takes no optional locks: it never rewrites the index.
git_tree_dirty() {
  local status
  if ! status="$(GIT_OPTIONAL_LOCKS=0 git -C "$1" status --porcelain=v1 --untracked-files=normal)"; then
    echo "cannot read the git status of $1" >&2
    return 1
  fi
  if [[ -n "$status" ]]; then echo true; else echo false; fi
}

# notarytool --wait limit for `xcrun notarytool submit … --timeout`, from
# BROSCRIBE_NOTARY_TIMEOUT (default 45m): a positive duration in s, m or h.
NOTARY_TIMEOUT_DEFAULT=45m
# Upload and status-polling time on top of the --wait limit before the outer
# wall-clock bound stops a stuck notarytool.
NOTARY_UPLOAD_GRACE_SECONDS=600

# Prints the validated BROSCRIBE_NOTARY_TIMEOUT, or fails.
checked_notary_timeout() {
  local value="${BROSCRIBE_NOTARY_TIMEOUT:-$NOTARY_TIMEOUT_DEFAULT}"
  if [[ ! "$value" =~ ^[1-9][0-9]{0,3}[smh]$ ]]; then
    echo 'BROSCRIBE_NOTARY_TIMEOUT must be a duration such as 45m.' >&2
    return 1
  fi
  printf '%s\n' "$value"
}

# Prints the wall-clock seconds one `notarytool submit --wait --timeout $1` may
# take in all: the --wait limit plus NOTARY_UPLOAD_GRACE_SECONDS.
notary_wall_seconds() {
  local value="$1" amount unit
  if [[ ! "$value" =~ ^([1-9][0-9]{0,3})([smh])$ ]]; then
    echo "not a notarytool duration: $value" >&2
    return 1
  fi
  amount="${BASH_REMATCH[1]}"
  unit="${BASH_REMATCH[2]}"
  case "$unit" in
    s) ;;
    m) amount=$((amount * 60)) ;;
    h) amount=$((amount * 3600)) ;;
  esac
  echo $((amount + NOTARY_UPLOAD_GRACE_SECONDS))
}

# Runs a native tool for at most $1 seconds: exit 124 when it has to be stopped.
bounded() {
  python3 -c 'import subprocess, sys
try:
    sys.exit(subprocess.run(sys.argv[2:], timeout=int(sys.argv[1])).returncode)
except subprocess.TimeoutExpired:
    print(f"{sys.argv[2]} did not finish within {sys.argv[1]} s", file=sys.stderr)
    sys.exit(124)' "$@"
}
