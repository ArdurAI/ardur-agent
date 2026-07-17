#!/bin/bash
# Batch-fix PR branches: reset GitHub's unsigned merge, merge dev with DCO, push
# Usage: ./fix-pr-branches.sh 299 300 302 ...

set -euo pipefail

cd /Users/gnutakki16/ardur-agent

for pr in "$@"; do
  echo "=== Processing PR #$pr ==="
  
  # Get branch name
  branch=$(gh pr view "$pr" --repo ArdurAI/ardur-agent --json headRefName --jq '.headRefName')
  if [ -z "$branch" ]; then
    echo "SKIP: could not get branch for #$pr"
    continue
  fi
  echo "Branch: $branch"
  
  # Fetch and checkout
  git fetch origin "$branch"
  git checkout "$branch" 2>/dev/null || git checkout -b "$branch" "origin/$branch"
  git pull origin "$branch" --ff-only 2>/dev/null || true
  
  # Find the last signed commit before GitHub's unsigned merges
  # Look for merge commits with "Merge branch 'dev'" that lack Signed-off-by
  base=$(git log --format="%H %(trailers:key=Signed-off-by,valueonly)" --grep="Merge branch 'dev'" "$branch" | awk 'NF==2 {print $1; exit}')
  if [ -z "$base" ]; then
    # No signed merge found; use the first non-merge commit before any merge
    base=$(git log --format="%H" --no-merges "$branch" | tail -1)
  fi
  
  if [ -z "$base" ]; then
    echo "SKIP: no base found for #$pr"
    continue
  fi
  
  echo "Resetting to signed base: $base"
  git reset --hard "$base"
  
  # Merge dev with signoff
  echo "Merging origin/dev with DCO..."
  if git merge origin/dev --no-ff -m "Merge branch 'dev' into $branch

Signed-off-by: Ardur <team@ardur.ai>"; then
    echo "Pushing..."
    git push origin "$branch" --force-with-lease
    echo "OK: #$pr"
  else
    echo "CONFLICT: #$pr needs manual conflict resolution"
    git merge --abort 2>/dev/null || true
  fi
  
  echo ""
done
