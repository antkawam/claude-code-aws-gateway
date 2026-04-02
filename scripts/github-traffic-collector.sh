#!/usr/bin/env bash
# github-traffic-collector.sh
#
# Collects GitHub traffic metrics (clones, views, referrers, paths, repo stats)
# and persists them beyond GitHub's 14-day retention window.
#
# Deduplicates overlapping data points on each run. Safe to run multiple
# times per day — designed for twice-daily cron.
#
# Requirements: gh (authenticated), jq
# Usage:       ./github-traffic-collector.sh
# Config via env vars:
#   GITHUB_TRAFFIC_REPO     — owner/repo (default: antkawam/claude-code-aws-gateway)
#   GITHUB_TRAFFIC_DATA_DIR — where to store JSON files
#   GITHUB_TRAFFIC_LOG      — log file path

set -euo pipefail

# ---------------------------------------------------------------------------
# Configuration
# ---------------------------------------------------------------------------
REPO="${GITHUB_TRAFFIC_REPO:-antkawam/claude-code-aws-gateway}"
OWNER="${REPO%%/*}"
REPONAME="${REPO##*/}"
DATA_DIR="${GITHUB_TRAFFIC_DATA_DIR:-$HOME/.local/share/github-traffic/$OWNER/$REPONAME}"
LOG_FILE="${GITHUB_TRAFFIC_LOG:-$DATA_DIR/collector.log}"

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
log "=== Collection run for $REPO ==="

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

# Merge per-day time-series (clones, views).
# Deduplicates by .timestamp; newer values win for same timestamp.
merge_timeseries() {
    local file="$1" new_data="$2"

    if [[ -s "$file" ]]; then
        jq -s '
            (.[0] + .[1])
            | group_by(.timestamp)
            | map(last)
            | sort_by(.timestamp)
        ' "$file" <(echo "$new_data") > "${file}.tmp"
    else
        echo "$new_data" | jq 'sort_by(.timestamp)' > "${file}.tmp"
    fi
    mv "${file}.tmp" "$file"
}

# Store one snapshot per date for aggregate endpoints (referrers, paths).
merge_snapshots() {
    local file="$1" new_data="$2"
    local today
    today=$(date -u '+%Y-%m-%d')

    local snapshot
    snapshot=$(jq -n --arg date "$today" --argjson data "$new_data" \
        '{date: $date, data: $data}')

    if [[ -s "$file" ]]; then
        jq --arg today "$today" --argjson snap "$snapshot" '
            [.[] | select(.date != $today)] + [$snap]
            | sort_by(.date)
        ' "$file" > "${file}.tmp"
    else
        jq -n --argjson snap "$snapshot" '[$snap]' > "${file}.tmp"
    fi
    mv "${file}.tmp" "$file"
}

# Store one point-in-time snapshot per date for repo stats.
merge_stats() {
    local file="$1" new_data="$2"
    local today
    today=$(date -u '+%Y-%m-%d')

    local snapshot
    snapshot=$(echo "$new_data" | jq --arg date "$today" '{
        date: $date,
        stargazers: .stargazers_count,
        forks: .forks_count,
        open_issues: .open_issues_count,
        subscribers: .subscribers_count,
        size_kb: .size
    }')

    if [[ -s "$file" ]]; then
        jq --arg today "$today" --argjson snap "$snapshot" '
            [.[] | select(.date != $today)] + [$snap]
            | sort_by(.date)
        ' "$file" > "${file}.tmp"
    else
        jq -n --argjson snap "$snapshot" '[$snap]' > "${file}.tmp"
    fi
    mv "${file}.tmp" "$file"
}

errors=0

# ---------------------------------------------------------------------------
# Collect: clones
# ---------------------------------------------------------------------------
log "Fetching clones..."
if clones_raw=$(gh api "repos/$REPO/traffic/clones" 2>&1); then
    clones_data=$(echo "$clones_raw" | jq '.clones')
    fetched=$(echo "$clones_data" | jq 'length')
    merge_timeseries "$DATA_DIR/clones.json" "$clones_data"
    total=$(jq 'length' "$DATA_DIR/clones.json")
    log "  Clones: $fetched new points, $total total stored"
else
    log "  ERROR (clones): $clones_raw"
    ((errors++)) || true
fi

# ---------------------------------------------------------------------------
# Collect: views
# ---------------------------------------------------------------------------
log "Fetching views..."
if views_raw=$(gh api "repos/$REPO/traffic/views" 2>&1); then
    views_data=$(echo "$views_raw" | jq '.views')
    fetched=$(echo "$views_data" | jq 'length')
    merge_timeseries "$DATA_DIR/views.json" "$views_data"
    total=$(jq 'length' "$DATA_DIR/views.json")
    log "  Views: $fetched new points, $total total stored"
else
    log "  ERROR (views): $views_raw"
    ((errors++)) || true
fi

# ---------------------------------------------------------------------------
# Collect: referrers
# ---------------------------------------------------------------------------
log "Fetching referrers..."
if referrers_raw=$(gh api "repos/$REPO/traffic/popular/referrers" 2>&1); then
    count=$(echo "$referrers_raw" | jq 'length')
    merge_snapshots "$DATA_DIR/referrers.json" "$referrers_raw"
    log "  Referrers: $count sources captured"
else
    log "  ERROR (referrers): $referrers_raw"
    ((errors++)) || true
fi

# ---------------------------------------------------------------------------
# Collect: paths
# ---------------------------------------------------------------------------
log "Fetching paths..."
if paths_raw=$(gh api "repos/$REPO/traffic/popular/paths" 2>&1); then
    count=$(echo "$paths_raw" | jq 'length')
    merge_snapshots "$DATA_DIR/paths.json" "$paths_raw"
    log "  Paths: $count entries captured"
else
    log "  ERROR (paths): $paths_raw"
    ((errors++)) || true
fi

# ---------------------------------------------------------------------------
# Collect: repo stats (stars, forks — not ephemeral, but nice to trend)
# ---------------------------------------------------------------------------
log "Fetching repo stats..."
if repo_raw=$(gh api "repos/$REPO" 2>&1); then
    stars=$(echo "$repo_raw" | jq '.stargazers_count')
    forks=$(echo "$repo_raw" | jq '.forks_count')
    merge_stats "$DATA_DIR/repo_stats.json" "$repo_raw"
    log "  Repo: $stars stars, $forks forks"
else
    log "  ERROR (repo stats): $repo_raw"
    ((errors++)) || true
fi

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------
if [[ "$errors" -gt 0 ]]; then
    log "=== Done with $errors error(s) ==="
    exit 1
else
    log "=== Done — all metrics collected ==="
fi
