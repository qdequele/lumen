#!/usr/bin/env bash
# Prepare a LUMEN release: bump the workspace version, keep every copy of it
# in sync, and fold the CHANGELOG's [Unreleased] section into a dated one.
#
#   scripts/prepare-release.sh                 suggest a bump from the commits since the last tag (changes nothing)
#   scripts/prepare-release.sh auto            apply that suggestion
#   scripts/prepare-release.sh patch|minor|major|X.Y.Z
#                                              prepare that release
#   scripts/prepare-release.sh --check         fail when Cargo.toml, Cargo.lock and docs/openapi.yaml disagree
#   scripts/prepare-release.sh --notes X.Y.Z   print the CHANGELOG section of X.Y.Z
#
# A release edits, together or not at all:
#   1. Cargo.toml `[workspace.package] version` and the workspace crates in Cargo.lock;
#   2. docs/openapi.yaml `info.version` (a test pins it to the crate version);
#   3. CHANGELOG.md: `## [Unreleased]` becomes `## [X.Y.Z] - YYYY-MM-DD` under a
#      fresh empty [Unreleased], and the compare-link footer follows.
#
# Bumps are computed from the last `vX.Y.Z` tag, not from Cargo.toml, so the
# script is idempotent: re-running `patch` after a partial or complete run
# finishes or confirms the same release instead of bumping twice. It refuses
# an empty [Unreleased] section and a version whose tag already exists.
#
# Suggestion rule (conventional commits since the last tag, merges ignored):
# a `type!:` subject or a `BREAKING CHANGE` footer suggests major, any `feat`
# suggests minor, anything else patch.
#
# RELEASE_DATE=YYYY-MM-DD overrides today's (UTC) date. In GitHub Actions the
# prepared version is written to $GITHUB_OUTPUT as `version` and `tag`.
set -euo pipefail

cd "$(dirname "$0")/.."

CARGO_TOML=Cargo.toml
CARGO_LOCK=Cargo.lock
OPENAPI=docs/openapi.yaml
CHANGELOG=CHANGELOG.md
SEMVER_RE='^[0-9]+\.[0-9]+\.[0-9]+$'

die() { echo "prepare-release: $*" >&2; exit 1; }

usage() {
  sed -n '2,11p' "$0" | sed 's/^# \{0,1\}//'
}

# --- readers -----------------------------------------------------------------

crate_version() {
  awk '
    /^\[workspace\.package\]/ { inside = 1; next }
    /^\[/ { inside = 0 }
    inside && /^version[ \t]*=/ { v = $0; sub(/^[^"]*"/, "", v); sub(/".*$/, "", v); print v; exit }
  ' "$CARGO_TOML"
}

openapi_version() {
  awk '
    /^info:/ { inside = 1; next }
    /^[^ \t#]/ { inside = 0 }
    inside && /^  version:/ { v = $2; gsub(/["\047]/, "", v); print v; exit }
  ' "$OPENAPI"
}

# "name version" for every Cargo.lock package without a `source`: the
# workspace's own crates (path packages).
lock_workspace_versions() {
  awk '
    function flush() { if (name != "" && !src) print name, ver; name = ""; ver = ""; src = 0 }
    /^\[\[package\]\]/ { flush(); next }
    /^name = / { name = $3; gsub(/"/, "", name) }
    /^version = / { ver = $3; gsub(/"/, "", ver) }
    /^source = / { src = 1 }
    END { flush() }
  ' "$CARGO_LOCK"
}

last_tag() {
  git describe --tags --abbrev=0 --match 'v[0-9]*.[0-9]*.[0-9]*' 2>/dev/null || true
}

# Lines between `## [Unreleased]` and the next `## [` heading.
unreleased_body() {
  awk '
    index($0, "## [Unreleased]") == 1 { found = 1; next }
    /^## \[/ { if (found) exit }
    found { print }
  ' "$CHANGELOG"
}

# Lines of the `## [X.Y.Z]` section (the same extraction release.yml uses).
section_body() {
  awk -v ver="$1" '
    index($0, "## [" ver "]") == 1 { found = 1; next }
    /^## \[/ { found = 0 }
    found { print }
  ' "$CHANGELOG"
}

has_section() {
  grep -q "^## \[$(printf '%s' "$1" | sed 's/\./\\./g')\]" "$CHANGELOG"
}

# --- semver ------------------------------------------------------------------

# version_gt A B: A > B
version_gt() {
  local IFS=.
  # shellcheck disable=SC2206
  local a=($1) b=($2) i
  for i in 0 1 2; do
    if [ "${a[$i]}" -gt "${b[$i]}" ]; then return 0; fi
    if [ "${a[$i]}" -lt "${b[$i]}" ]; then return 1; fi
  done
  return 1
}

bump() {
  local base="$1" kind="$2" major minor patch
  IFS=. read -r major minor patch <<EOF
$base
EOF
  case "$kind" in
    major) echo "$((major + 1)).0.0" ;;
    minor) echo "$major.$((minor + 1)).0" ;;
    patch) echo "$major.$minor.$((patch + 1))" ;;
    *) die "unknown bump '$kind'" ;;
  esac
}

# Prints major|minor|patch from the commits since $1 (a tag, or empty for all
# history), or `none` when there are no commits.
suggest_bump() {
  local range=HEAD
  [ -n "$1" ] && range="$1..HEAD"
  git log --no-merges --format='%s%x1f%b%x1e' "$range" | awk '
    BEGIN { RS = "\036"; FS = "\037"; level = 0; n = 0 }
    {
      subject = $1; body = $2
      sub(/^[\n\r]+/, "", subject)
      if (subject == "") next
      n++
      if (subject ~ /^[A-Za-z]+(\([^)]*\))?!:/ || body ~ /BREAKING[ -]CHANGE/ || subject ~ /BREAKING[ -]CHANGE/) { if (level < 3) level = 3 }
      else if (subject ~ /^feat(\([^)]*\))?:/) { if (level < 2) level = 2 }
      else if (level < 1) level = 1
    }
    END {
      if (n == 0) print "none"
      else if (level == 3) print "major"
      else if (level == 2) print "minor"
      else print "patch"
    }
  '
}

# --- commands ----------------------------------------------------------------

cmd_check() {
  local crate api status=0 name ver
  crate="$(crate_version)"
  api="$(openapi_version)"
  [ -n "$crate" ] || die "no [workspace.package] version in $CARGO_TOML"
  if [ "$api" != "$crate" ]; then
    echo "::error file=$OPENAPI::info.version is '$api' but the crate version ($CARGO_TOML) is '$crate'; run scripts/prepare-release.sh or fix it by hand" >&2
    status=1
  fi
  while read -r name ver; do
    [ -n "$name" ] || continue
    if [ "$ver" != "$crate" ]; then
      echo "::error file=$CARGO_LOCK::workspace crate '$name' is locked at '$ver' but the crate version is '$crate'; run cargo update --workspace" >&2
      status=1
    fi
  done <<EOF
$(lock_workspace_versions)
EOF
  [ "$status" -eq 0 ] && echo "version $crate is consistent across $CARGO_TOML, $CARGO_LOCK and $OPENAPI"
  return "$status"
}

cmd_suggest() {
  local tag base kind
  tag="$(last_tag)"
  base="${tag#v}"
  [ -n "$base" ] || base="$(crate_version)"
  kind="$(suggest_bump "$tag")"
  if [ "$kind" = "none" ]; then
    echo "no commits since ${tag:-the first commit}; nothing to release"
    return 0
  fi
  echo "last tag: ${tag:-none} (base $base)"
  echo "commits since then:"
  if [ -n "$tag" ]; then git log --no-merges --format='  %s' "$tag..HEAD"; else git log --no-merges --format='  %s'; fi
  echo
  echo "suggested bump: $kind -> $(bump "$base" "$kind")"
  echo "apply it with: scripts/prepare-release.sh $kind   (or: scripts/prepare-release.sh auto)"
}

cmd_prepare() {
  local arg="$1" tag base target crate date url name
  tag="$(last_tag)"
  base="${tag#v}"
  crate="$(crate_version)"
  [ -n "$base" ] || base="$crate"
  [[ "$base" =~ $SEMVER_RE ]] || die "cannot read the base version (last tag '${tag:-none}', crate '$crate')"

  case "$arg" in
    auto)
      arg="$(suggest_bump "$tag")"
      [ "$arg" != "none" ] || die "no commits since ${tag:-the first commit}; nothing to release"
      echo "auto: commits since ${tag:-the first commit} suggest a $arg bump"
      target="$(bump "$base" "$arg")"
      ;;
    major | minor | patch) target="$(bump "$base" "$arg")" ;;
    *)
      arg="${arg#v}"
      [[ "$arg" =~ $SEMVER_RE ]] || die "expected patch, minor, major, auto or X.Y.Z, got '$1'"
      target="$arg"
      ;;
  esac

  if git rev-parse -q --verify "refs/tags/v$target" >/dev/null; then
    die "tag v$target already exists; pick the next version"
  fi
  version_gt "$target" "$base" || die "$target is not greater than the last release ($base)"

  date="${RELEASE_DATE:-$(date -u +%Y-%m-%d)}"
  [[ "$date" =~ ^[0-9]{4}-[0-9]{2}-[0-9]{2}$ ]] || die "RELEASE_DATE must be YYYY-MM-DD, got '$date'"

  local fold_changelog=true
  if has_section "$target"; then
    # Already folded by an earlier run: only (re)sync the version copies.
    fold_changelog=false
  elif ! unreleased_body | grep -q '[^[:space:]]'; then
    die "the [Unreleased] section of $CHANGELOG is empty: add the release's entries first"
  fi

  if ! $fold_changelog && [ "$crate" = "$target" ] && [ "$(openapi_version)" = "$target" ] && cmd_check >/dev/null 2>&1; then
    echo "v$target is already prepared; nothing to change"
  else
    # Write every new file next to the originals, then swap them in at the
    # end: a failure before the swap leaves the tree untouched.
    tmp="$(mktemp -d "${TMPDIR:-/tmp}/prepare-release.XXXXXX")"
    trap 'rm -rf "$tmp"' EXIT

    awk -v ver="$target" '
      /^\[workspace\.package\]/ { inside = 1; print; next }
      /^\[/ { inside = 0 }
      inside && !done && /^version[ \t]*=/ { print "version = \"" ver "\""; done = 1; next }
      { print }
    ' "$CARGO_TOML" >"$tmp/Cargo.toml"

    awk -v ver="$target" '
      function flush(   i) {
        for (i = 1; i <= n; i++) {
          if (inpkg && !src && lines[i] ~ /^version = /) print "version = \"" ver "\""
          else print lines[i]
        }
        n = 0; src = 0
      }
      /^\[\[package\]\]/ { flush() ; inpkg = 1 }
      /^source = / { src = 1 }
      { lines[++n] = $0 }
      END { flush() }
    ' "$CARGO_LOCK" >"$tmp/Cargo.lock"

    awk -v ver="$target" '
      /^info:/ { inside = 1; print; next }
      /^[^ \t#]/ { inside = 0 }
      inside && !done && /^  version:/ { print "  version: " ver; done = 1; next }
      { print }
    ' "$OPENAPI" >"$tmp/openapi.yaml"

    if $fold_changelog; then
      url="$(sed -n 's|^\[Unreleased\]: \(.*\)/compare/.*$|\1|p' "$CHANGELOG" | head -n 1)"
      awk -v ver="$target" -v date="$date" -v url="$url" -v prev="$tag" '
        !heading && index($0, "## [Unreleased]") == 1 {
          print; print ""; print "## [" ver "] - " date; heading = 1; next
        }
        !footer && url != "" && index($0, "[Unreleased]: ") == 1 {
          print "[Unreleased]: " url "/compare/v" ver "...HEAD"
          if (prev != "") print "[" ver "]: " url "/compare/" prev "...v" ver
          else print "[" ver "]: " url "/releases/tag/v" ver
          footer = 1; next
        }
        { print }
      ' "$CHANGELOG" >"$tmp/CHANGELOG.md"
    else
      cp "$CHANGELOG" "$tmp/CHANGELOG.md"
    fi

    # Verify the rewritten copies before touching the tree.
    grep -q "^version = \"$target\"" "$tmp/Cargo.toml" || die "could not set the version in $CARGO_TOML"
    grep -q "^  version: $target\$" "$tmp/openapi.yaml" || die "could not set info.version in $OPENAPI"
    CHANGELOG="$tmp/CHANGELOG.md" has_section "$target" || die "could not fold [Unreleased] in $CHANGELOG"
    while read -r name _; do
      [ -n "$name" ] || continue
      CARGO_LOCK="$tmp/Cargo.lock" lock_workspace_versions | grep -qx "$name $target" || die "could not set $name in $CARGO_LOCK"
    done <<EOF
$(lock_workspace_versions)
EOF
    # Only the workspace crates' version lines may change in the lockfile.
    local expected changed
    expected="$(lock_workspace_versions | awk -v ver="$target" '$2 != ver' | wc -l | tr -d ' ')"
    changed="$( (diff "$CARGO_LOCK" "$tmp/Cargo.lock" || true) | grep -c '^>' || true)"
    [ "$changed" = "$expected" ] || die "$CARGO_LOCK rewrite changed $changed lines, expected $expected"

    mv "$tmp/Cargo.toml" "$CARGO_TOML"
    mv "$tmp/Cargo.lock" "$CARGO_LOCK"
    mv "$tmp/openapi.yaml" "$OPENAPI"
    mv "$tmp/CHANGELOG.md" "$CHANGELOG"
    echo "prepared v$target (was $crate; last tag ${tag:-none}):"
    echo "  $CARGO_TOML, $CARGO_LOCK, $OPENAPI -> $target"
    if $fold_changelog; then echo "  $CHANGELOG: [Unreleased] -> [$target] - $date"; fi
  fi

  cmd_check >/dev/null
  if [ -n "${GITHUB_OUTPUT:-}" ]; then
    { echo "version=$target"; echo "tag=v$target"; } >>"$GITHUB_OUTPUT"
  fi
  echo "next: commit, open the release PR titled 'chore(release): v$target', merge it; the tag and release follow."
}

cmd_notes() {
  local ver="${1#v}"
  [[ "$ver" =~ $SEMVER_RE ]] || die "--notes needs X.Y.Z"
  has_section "$ver" || die "$CHANGELOG has no [$ver] section"
  # Trim leading and trailing blank lines.
  section_body "$ver" | awk 'NF { started = 1 } started { buf[++n] = $0 } END { while (n > 0 && buf[n] ~ /^[[:space:]]*$/) n--; for (i = 1; i <= n; i++) print buf[i] }'
}

case "${1:-}" in
  "") cmd_suggest ;;
  -h | --help) usage ;;
  --check) cmd_check ;;
  --notes) cmd_notes "${2:-}" ;;
  -*) usage >&2; exit 2 ;;
  *)
    [ "$#" -eq 1 ] || { usage >&2; exit 2; }
    cmd_prepare "$1"
    ;;
esac
