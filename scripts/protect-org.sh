#!/bin/sh
# Every repository of the meadow-lang organisation takes pull requests on its
# default branch and nothing else: no direct push, no force push, no deletion,
# for administrators too. Run it again after making a repository -- the
# organisation is on GitHub's free plan, which has no ruleset for a whole
# organisation, so a new repository is unprotected until this is run. It
# changes nothing on one that already has the ruleset.
#
#   scripts/protect-org.sh            every repository
#   scripts/protect-org.sh Name ...   those named
set -eu
ORG=meadow-lang
NAME="default branch takes pull requests"
RULES='{
  "name": "default branch takes pull requests",
  "target": "branch",
  "enforcement": "active",
  "conditions": { "ref_name": { "include": ["~DEFAULT_BRANCH"], "exclude": [] } },
  "rules": [
    { "type": "deletion" },
    { "type": "non_fast_forward" },
    { "type": "pull_request",
      "parameters": {
        "required_approving_review_count": 0,
        "dismiss_stale_reviews_on_push": false,
        "require_code_owner_review": false,
        "require_last_push_approval": false,
        "required_review_thread_resolution": false } }
  ]
}'
if [ "$#" -gt 0 ]; then repos="$*"; else repos=$(gh repo list "$ORG" --limit 500 --json name --jq '.[].name'); fi
for r in $repos; do
  if gh api "repos/$ORG/$r/rulesets" --jq '.[].name' 2>/dev/null | grep -qx "$NAME"; then
    echo "$r: already protected"
  elif out=$(printf '%s' "$RULES" | gh api -X POST "repos/$ORG/$r/rulesets" --input - 2>&1); then
    echo "$r: protected"
  else
    echo "$r: NOT protected: $(printf '%s' "$out" | head -1 | cut -c1-160)"
  fi
done
