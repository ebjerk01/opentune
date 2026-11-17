#!/usr/bin/env bash
# Point this clone at the tracked git hooks in .githooks/ (fast fmt + JS syntax
# check on commit). Run once per clone; undo with `git config --unset core.hooksPath`.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
git -C "$ROOT" config core.hooksPath .githooks
chmod +x "$ROOT"/.githooks/* 2>/dev/null || true
echo "git hooks enabled: core.hooksPath = .githooks"
