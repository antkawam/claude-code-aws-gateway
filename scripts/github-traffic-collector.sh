#!/usr/bin/env bash
# github-traffic-collector.sh
#
# Persists GitHub traffic metrics (clones, views, stars, forks) as CSV
# beyond GitHub's 14-day retention window. Idempotent — only appends
# rows for dates not already recorded. Designed for twice-daily cron.
#
# Requirements: gh (authenticated), jq
# Usage:       ./github-traffic-collector.sh
# Config via env vars:
#   GITHUB_TRAFFIC_REPO     — owner/repo (default: antkawam/claude-code-aws-gateway)
#   GITHUB_TRAFFIC_DATA_DIR — where to store the CSV + log

set -euo pipefail

# ---------------------------------------------------------------------------
# Configuration
# ---------------------------------------------------------------------------
REPO="${GITHUB_TRAFFIC_REPO:-antkawam/claude-code-aws-gateway}"
OWNER="${REPO%%/*}"
REPONAME="${REPO##*/}"
DATA_DIR="${GITHUB_TRAFFIC_DATA_DIR:-$HOME/.local/share/github-traffic/$OWNER/$REPONAME}"
CSV="$DATA_DIR/traffic.csv"
LOG_FILE="$DATA_DIR/collector.log"
HEADER="date,views,views_uniques,clones,clones_uniques,stars,forks"

# ---------------------------------------------------------------------------
# Logging
# ---------------------------------------------------------------------------
log() { printf '[%s] %s\n' "$(date -u '+%Y-%m-%dT%H:%M:%SZ')" "$*" | tee -a "$LOG_FILE"; }
die() { log "FATAL: $*"; exit 1; }

# ---------------------------------------------------------------------------
# Preflight
# ---------------------------------------------------------------------------
command -v gh >/dev/null 2>&1  || die "gh CLI not found"
command -v jq >/dev/null 2>&1  || die "jq not found"
gh auth status >/dev/null 2>&1 || die "gh not authenticated — run 'gh auth login'"

mkdir -p "$DATA_DIR"

# Initialize CSV with header if missing
if [[ ! -f "$CSV" ]]; then
    echo "$HEADER" > "$CSV"
fi

log "=== Collection run for $REPO ==="

# ---------------------------------------------------------------------------
# Load existing dates (skip header)
# ---------------------------------------------------------------------------
declare -A existing_dates
while IFS=, read -r d _; do
    existing_dates["$d"]=1
done < <(tail -n +2 "$CSV")

# ---------------------------------------------------------------------------
# Fetch API data
# ---------------------------------------------------------------------------
errors=0
today=$(date -u '+%Y-%m-%d')

log "Fetching views..."
if views_raw=$(gh api "repos/$REPO/traffic/views" 2>&1); then
    log "  Views: $(echo "$views_raw" | jq '.views | length') days returned"
else
    log "  ERROR (views): $views_raw"; ((errors++)) || true; views_raw='{"views":[]}'
fi

log "Fetching clones..."
if clones_raw=$(gh api "repos/$REPO/traffic/clones" 2>&1); then
    log "  Clones: $(echo "$clones_raw" | jq '.clones | length') days returned"
else
    log "  ERROR (clones): $clones_raw"; ((errors++)) || true; clones_raw='{"clones":[]}'
fi

log "Fetching repo stats..."
stars="" forks=""
if repo_raw=$(gh api "repos/$REPO" 2>&1); then
    stars=$(echo "$repo_raw" | jq -r '.stargazers_count')
    forks=$(echo "$repo_raw" | jq -r '.forks_count')
    log "  Repo: $stars stars, $forks forks"
else
    log "  ERROR (repo stats): $repo_raw"; ((errors++)) || true
fi

# ---------------------------------------------------------------------------
# Merge views + clones by date, append missing rows
# ---------------------------------------------------------------------------
# Build a lookup: date -> views,views_uniques,clones,clones_uniques
merged=$(jq -n \
    --argjson views "$views_raw" \
    --argjson clones "$clones_raw" '
    [
        # Index clones by date
        ($clones.clones | map({key: (.timestamp[:10]), value: .}) | from_entries) as $c |
        # Index views by date
        ($views.views   | map({key: (.timestamp[:10]), value: .}) | from_entries) as $v |
        # Union of all dates
        ([($c | keys[]), ($v | keys[])] | unique | sort)[] |
        . as $d |
        {
            date: $d,
            views:          (($v[$d].count)   // 0),
            views_uniques:  (($v[$d].uniques) // 0),
            clones:         (($c[$d].count)   // 0),
            clones_uniques: (($c[$d].uniques) // 0)
        }
    ]
')

added=0
for row in $(echo "$merged" | jq -c '.[]'); do
    d=$(echo "$row" | jq -r '.date')
    # Skip dates already in CSV
    if [[ -n "${existing_dates[$d]:-}" ]]; then
        continue
    fi

    v=$(echo "$row" | jq -r '.views')
    vu=$(echo "$row" | jq -r '.views_uniques')
    c=$(echo "$row" | jq -r '.clones')
    cu=$(echo "$row" | jq -r '.clones_uniques')

    # Only include stars/forks for today (point-in-time, not historical)
    if [[ "$d" == "$today" ]]; then
        echo "$d,$v,$vu,$c,$cu,$stars,$forks" >> "$CSV"
    else
        echo "$d,$v,$vu,$c,$cu,," >> "$CSV"
    fi
    ((added++)) || true
done

# Sort CSV by date (keep header at top)
{ head -1 "$CSV"; tail -n +2 "$CSV" | sort -t, -k1,1; } > "${CSV}.tmp"
mv "${CSV}.tmp" "$CSV"

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------
total=$(($(wc -l < "$CSV") - 1))
log "  Added $added new rows ($total total)"

if [[ "$errors" -gt 0 ]]; then
    log "=== Done with $errors error(s) ==="
    exit 1
else
    log "=== Done ==="
fi
