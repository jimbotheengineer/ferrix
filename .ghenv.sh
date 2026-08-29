#!/usr/bin/env bash
export PATH="$HOME/.cargo/bin:$PATH:/c/Program Files/GitHub CLI"
export HERMES_HOME="/c/Users/Error/AppData/Local/hermes"
source "$HERMES_HOME/profiles/rust-dev/skills/github/github-auth/scripts/gh-env.sh" >/dev/null 2>&1
export GH_TOKEN="$GITHUB_TOKEN"
export GH_REPO="jimbotheengineer/ferrix"
export REPO="jimbotheengineer/ferrix"
