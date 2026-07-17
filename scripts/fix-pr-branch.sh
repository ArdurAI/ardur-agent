#!/bin/bash
# Fix PR branch: reset to last signed commit, merge dev with DCO, push
# Usage: ./fix-pr-branch.sh <pr-number>

set -euo pipefail

cd /Users/gnutakki16/ardur-agent

pr="$1"
branch=$(gh pr view "$pr" --repo ArdurAI/ardur-agent --json headRefName --jq '.headRefName')
if [ -z "$branch" ]; then
  echo "ERROR: could not get branch for #$pr"
  exit 1
fi

echo "PR #$pr -> $branch"

git fetch origin "$branch"
git checkout "$branch"
git pull origin "$branch" --ff-only 2>/dev/null || true

# Find the most recent commit that has a Signed-off-by trailer
# This is the last good commit before GitHub's unsigned merges
signed_base=$(git log --format="%H|%(trailers:key=Signed-off-by,valueonly)" "$branch" | awk -F'|' 'NF==2 && $2!="" {print $1; exit}' || true)

if [ -z "$signed_base" ]; then
  echo "ERROR: no signed commit found on $branch"
  exit 1
fi

echo "Signed base: $signed_base"

# Check if HEAD is already the signed base
head_sha=$(git rev-parse HEAD)
if [ "$head_sha" = "$signed_base" ]; then
  echo "HEAD is already signed base, just merging dev..."
else
  echo "Resetting from $head_sha to $signed_base"
  git reset --hard "$signed_base"
fi

# Merge dev with signoff
echo "Fetching latest origin/dev..."
git fetch origin dev

echo "Merging origin/dev..."
if git merge origin/dev --no-ff -m "Merge branch 'dev' into $branch

Signed-off-by: Ardur <team@ardur.ai>"; then
  echo "Pushing..."
  git push origin "$branch" --force-with-lease
  echo "OK: #$pr"
else
  echo "CONFLICT: #$pr needs manual resolution"
  git merge --abort 2>/dev/null || true
  exit 1
fi
