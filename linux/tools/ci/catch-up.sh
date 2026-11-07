#!/usr/bin/env bash
# Replays the port onto upstream's newest main, inside a checkout of the fork's
# main. Run by the fork's Linux workflow every hour.
#
# Prints "changed=true" with HEAD at the replayed commit, or "changed=false" when
# main is already on upstream's newest commit. Exits non-zero when a patch no
# longer applies; zephium-linux's repair step carries it over on the next local run.
set -euo pipefail

UPSTREAM=https://github.com/zephium-browser/Zephium.git

git fetch --quiet "$UPSTREAM" main
upstream=$(git rev-parse FETCH_HEAD)
published=$(git rev-parse HEAD)
if git merge-base --is-ancestor "$upstream" "$published"; then
  echo "changed=false"
  exit 0
fi

# What the port adds on top of upstream, as it is on the fork's main now.
port=$(mktemp -d)
git archive "$published" linux .github/README.md .github/linux .github/workflows | tar -x -C "$port"

# Same identity as the published commits.
git config user.name "$(git log -1 --format=%an "$published")"
git config user.email "$(git log -1 --format=%ae "$published")"

git checkout --quiet --detach "$upstream"
git am --quiet --3way "$port"/linux/patches/*.patch >&2

rm -rf linux .github/linux .github/workflows
cp -a "$port"/linux "$port"/.github .
git add --all linux .github/README.md .github/linux .github/workflows
git commit --quiet --message "$(git log -1 --format=%B "$published")"

echo "changed=true"
echo "upstream=$upstream"
