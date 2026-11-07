#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."

# A submodule's .git is a gitlink *file* (not a directory), so test -e, not -d.
if [[ ! -e upstream/libslic3r/.git ]]; then
    echo "upstream/libslic3r submodule not yet present (Task T1A.2)."
    echo "Skipping patch application."
    exit 0
fi

# NOTE: this resets the submodule working tree but does NOT touch the out-of-tree
# build dirs (deps/build, build) -- those are untracked and would be removed by a
# `git clean`, so we intentionally don't clean here (CI caches them). A fresh
# checkout has nothing to clean anyway.
( cd upstream/libslic3r && git reset --hard HEAD )

shopt -s nullglob
for patch in patches/0[0-9][0-9][0-9]-*.patch; do
    [[ -f "$patch" ]] || continue
    name="$(basename "$patch")"
    if ( cd upstream/libslic3r && git apply --reverse --check "../../$patch" ) >/dev/null 2>&1; then
        # Already present (e.g. folded into the pinned submodule commit).
        echo "Skipping $name (already applied)"
    elif ( cd upstream/libslic3r && git apply --check "../../$patch" ) >/dev/null 2>&1; then
        echo "Applying $name"
        ( cd upstream/libslic3r && git apply "../../$patch" )
    else
        echo "WARNING: $name neither applies cleanly nor is already applied; skipping"
    fi
done

echo "Patch application complete."
