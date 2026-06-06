#!/usr/bin/env bash
# check-docs.sh — Lint public docs for internal spec/task language leaks.
#
# WHY: Specs and task lists use "Task N", "Slice M", "T7", "R1" as internal
# references. When prose is copy-pasted from a spec into `docs/*.md`, those
# references can leak. They mean nothing to public readers and read as
# unfinished writing. Caught one ("see Tasks 7-8" in docs/configuration.md)
# during the v1.8.0 review — close the door on it.
#
# USAGE:
#   .claude/scripts/check-docs.sh
#
# Exits 0 if clean, 1 if any matches found. Suitable for `make check` / CI.
#
# What it flags:
#   - "see Task 7" / "see Tasks 7-8"
#   - "Slice 3"
#   - bare T7, T1, R1 (capital letter + digits) anywhere a public reader
#     would not understand it
#
# What it does NOT flag:
#   - URLs, file paths, code blocks (we filter common false-positive shapes)

set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

# Files in scope: tracked .md files outside .claude/ (which is gitignored anyway)
# and outside vendor/build dirs.
FILES=$(git ls-files '*.md' | grep -vE '^\.claude/|^node_modules/|^target/|^infra/cdk\.out/|^tests/frontend/(node_modules|test-results|playwright-report)/' || true)

if [ -z "$FILES" ]; then
    echo "no public .md files found"
    exit 0
fi

# Patterns. Use `grep -E` with word boundaries; `-n` for line numbers.
# Filter out matches inside fenced code blocks crudely (skip lines starting
# with whitespace + backtick or pure code-fence). For richer filtering we'd
# need a real markdown parser; keeping it simple.
PATTERNS='\bsee Tasks? [0-9]|\bSlice [0-9]+\b|\b[TR][0-9]+\b'

# `git grep` respects .gitignore and is fast.
hits=$(git grep -nE "$PATTERNS" -- $FILES 2>/dev/null \
    | grep -vE ':\s*```' \
    | grep -vE '\.tar\.|\.tgz|\.zip' \
    || true)

if [ -n "$hits" ]; then
    echo "ERROR: internal spec/task language found in public docs:" >&2
    echo "" >&2
    echo "$hits" >&2
    echo "" >&2
    echo "Replace 'Task N' / 'Slice M' / 'T7' / 'R1' references with:" >&2
    echo "  - the actual command/behavior being described, OR" >&2
    echo "  - a link to the spec/issue if appropriate, OR" >&2
    echo "  - remove the reference if it was internal-only." >&2
    exit 1
fi

echo "docs lint: clean — no internal spec/task language found"
