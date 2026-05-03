#!/usr/bin/env bash
# release.sh — Bump plugin version, commit, tag and push to trigger the GitHub release workflow.
# Usage:
#   ./release.sh           # auto-increment patch  (0.1.0 -> 0.1.1)
#   ./release.sh minor     # auto-increment minor  (0.1.0 -> 0.2.0)
#   ./release.sh major     # auto-increment major  (0.1.0 -> 1.0.0)
#   ./release.sh 0.2.3     # explicit version

set -euo pipefail

PLUGIN_ROOT="plugins"
ACTIONS_URL="https://github.com/redbug26/kkc-plugins-rust/actions"

manifest_version() {
  grep '^version = ' "$1" | head -1 | sed 's/version = "\(.*\)"/\1/'
}

plugin_dirs=()
while IFS= read -r dir; do
  plugin_dirs+=("$dir")
done < <(find "$PLUGIN_ROOT" -mindepth 1 -maxdepth 1 -type d | sort)

if [[ ${#plugin_dirs[@]} -eq 0 ]]; then
  echo "No plugin directories found under ${PLUGIN_ROOT}/"
  exit 1
fi

manifest_files=()
cargo_files=()
plugin_names=()
current=""

# -- Read current versions --------------------------------------------------
for dir in "${plugin_dirs[@]}"; do
  manifest="${dir}/plugin.toml"
  cargo_toml="${dir}/Cargo.toml"

  if [[ ! -f "$manifest" || ! -f "$cargo_toml" ]]; then
    echo "Skipping ${dir}: missing plugin.toml or Cargo.toml"
    continue
  fi

  mver=$(manifest_version "$manifest")
  cver=$(manifest_version "$cargo_toml")
  if [[ -z "$mver" || -z "$cver" ]]; then
    echo "Cannot read versions from ${manifest} or ${cargo_toml}"
    exit 1
  fi
  if [[ "$mver" != "$cver" ]]; then
    echo "Version mismatch detected:"
    echo "  ${manifest} : ${mver}"
    echo "  ${cargo_toml}: ${cver}"
    echo "Please align versions first."
    exit 1
  fi

  if [[ -z "$current" ]]; then
    current="$mver"
  elif [[ "$mver" != "$current" ]]; then
    echo "All Rust plugin versions must match for a single release tag."
    echo "  Expected: ${current}"
    echo "  Found   : ${mver} in ${manifest}"
    exit 1
  fi

  manifest_files+=("$manifest")
  cargo_files+=("$cargo_toml")
  plugin_names+=("$(basename "$dir")")
done

if [[ ${#manifest_files[@]} -eq 0 ]]; then
  echo "No releasable Rust plugin found (plugin.toml + Cargo.toml) under ${PLUGIN_ROOT}/"
  exit 1
fi

IFS='.' read -r major minor patch <<< "$current"

# -- Compute new version ----------------------------------------------------
arg="${1:-patch}"
case "$arg" in
  major)
    new_version="$((major + 1)).0.0" ;;
  minor)
    new_version="${major}.$((minor + 1)).0" ;;
  patch)
    new_version="${major}.${minor}.$((patch + 1))" ;;
  [0-9]*.[0-9]*.[0-9]*)
    new_version="$arg" ;;
  *)
    echo "Usage: $0 [major|minor|patch|X.Y.Z]"
    exit 1 ;;
esac

echo "Current version : $current"
echo "New version     : $new_version"
echo "Plugins         : ${plugin_names[*]}"
echo ""

# -- Confirm ----------------------------------------------------------------
read -r -p "Proceed? [y/N] " confirm
[[ "$confirm" =~ ^[yY]$ ]] || { echo "Aborted."; exit 0; }

# -- Commit any pending changes first --------------------------------------
if [[ -n "$(git status --porcelain)" ]]; then
  echo ""
  echo "Pending changes detected - committing before bump:"
  git status --short
  git add -A
  git commit -m "chore: pre-release"
  git push origin main
  echo "Pre-release commit pushed"
fi

# -- Bump version in manifests ---------------------------------------------
for manifest in "${manifest_files[@]}"; do
  sed -i '' "s/^version = \"${current}\"/version = \"${new_version}\"/" "$manifest"
done
for cargo_toml in "${cargo_files[@]}"; do
  sed -i '' "s/^version = \"${current}\"/version = \"${new_version}\"/" "$cargo_toml"
done
echo "Versions updated"

# -- Verify build -----------------------------------------------------------
echo ""
echo "Building release plugins to verify..."
cargo build --release --workspace 2>&1 | tail -5
echo "Build OK"

# -- Commit bump ------------------------------------------------------------
git add "${manifest_files[@]}" "${cargo_files[@]}"
git commit -m "chore: bump rust plugins to v${new_version}"
git push origin main
echo "Commit pushed"

# -- Tag and push -----------------------------------------------------------
git tag "v${new_version}"
git push origin "v${new_version}"
echo "Tag v${new_version} pushed"

echo ""
echo "Release v${new_version} triggered"
echo "Follow progress at: ${ACTIONS_URL}"
echo ""