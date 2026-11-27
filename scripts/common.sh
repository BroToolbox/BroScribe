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
