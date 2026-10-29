#!/usr/bin/env bash
# Bumps the Super Shuckie version. The one place it is set is [workspace.package] in the root
# Cargo.toml: every crate inherits it (version.workspace = true) and supershuckie-qt/CMakeLists.txt
# reads it for the title bar. Cargo.lock is updated to match so cargo does not rewrite it later.
#
#   scripts/bump-version.sh          0.5.1 -> 0.5.2
#   scripts/bump-version.sh 0.6.0    set an exact version
#
# Prints the new version. The pre-commit hook in .githooks runs this on every commit.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
toml="$root/Cargo.toml"
lock="$root/Cargo.lock"

current_version() {
    awk '/^\[/ { in_package = ($0 ~ /^\[workspace\.package\]/) }
         in_package && /^version = "/ { split($0, part, "\""); print part[2]; exit }' "$toml" | tr -d '\r'
}

old="$(current_version)"
if [[ -z "$old" ]]; then
    echo "bump-version: no [workspace.package] version in $toml" >&2
    exit 1
fi

if [[ $# -ge 1 ]]; then
    new="$1"
elif [[ "$old" =~ ^([0-9]+)\.([0-9]+)\.([0-9]+)$ ]]; then
    new="${BASH_REMATCH[1]}.${BASH_REMATCH[2]}.$((BASH_REMATCH[3] + 1))"
else
    echo "bump-version: '$old' is not MAJOR.MINOR.PATCH; pass the new version explicitly" >&2
    exit 1
fi

if [[ ! "$new" =~ ^[0-9A-Za-z.+-]+$ ]]; then
    echo "bump-version: '$new' is not a valid version" >&2
    exit 1
fi

# Cargo.toml: only the version line inside [workspace.package].
awk -v new="$new" '
    /^\[/ { in_package = ($0 ~ /^\[workspace\.package\]/) }
    in_package && !done && /^version = "/ { sub(/"[^"]*"/, "\"" new "\""); done = 1 }
    { print }' "$toml" > "$toml.tmp" && mv "$toml.tmp" "$toml"

# Cargo.lock: the workspace's own packages are the ones without a source line.
if [[ -f "$lock" ]]; then
    awk -v old="$old" -v new="$new" '
        function flush() {
            if (n == 0) return
            local_package = 1
            for (i = 1; i <= n; i++) if (block[i] ~ /^source = /) local_package = 0
            for (i = 1; i <= n; i++) {
                if (local_package && block[i] ~ ("^version = \"" old "\"")) sub(/"[^"]*"/, "\"" new "\"", block[i])
                print block[i]
            }
            n = 0
        }
        /^\[\[package\]\]/ { flush() }
        { block[++n] = $0 }
        END { flush() }' "$lock" > "$lock.tmp" && mv "$lock.tmp" "$lock"
fi

echo "$new"
