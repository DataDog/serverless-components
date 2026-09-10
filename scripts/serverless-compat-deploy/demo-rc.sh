#!/usr/bin/env bash
set -euo pipefail

# ---------------------------------------------------------------------------
# demo-rc.sh — SVLS-9604 load test for serverless_compat_agent
#
# Triggers the compat functions N times in parallel waves to validate:
#   - Row count stays at 1 per resource_id (not per cold start or instance)
#   - EPRW write/rejection metrics are visible in staging
#   - Intake lag is measurable from trigger time to REDAPL query time
#   - Concurrent payloads don't cause key collisions or duplicate rows
#
# Load stages (per RFC Section 9):
#   L0 — 1 trigger (smoke test)
#   L1 — 10 concurrent triggers
#   L2 — 50 concurrent triggers (default, set LOAD_STAGE=L2)
#   L3 — 100 concurrent triggers (requires explicit LOAD_STAGE=L3)
#
# Usage:
#   export DD_API_KEY=<staging-key>
#   ./demo-rc.sh
#   ./demo-rc.sh LOAD_STAGE=L2
#   LOAD_STAGE=L3 ./demo-rc.sh
#
# Output:
#   - Realtime trigger log with per-request timing
#   - Per-stage summary: attempts, successes, failures, p50/p95/p99 latency
#   - EPRW metric queries to watch in the staging Datadog dashboard
#   - DDSQL queries to verify row cardinality after load
# ---------------------------------------------------------------------------

: "${DD_API_KEY:?DD_API_KEY must be set (staging key for datad0g.com)}"

DD_SITE="${DD_SITE:-datad0g.com}"
GCP_PROJECT="${GCP_PROJECT:-datadog-serverless-gcp-demo}"
GCP_REGION="${GCP_REGION:-us-central1}"
AZURE_FUNCTION_APP="${AZURE_FUNCTION_APP:-nina-compat-inventory-node}"
GCP_FUNCTION_NAME="${GCP_FUNCTION_NAME:-nina-compat-inventory-nodejs}"
LOAD_STAGE="${LOAD_STAGE:-L1}"
RESULTS_DIR="${RESULTS_DIR:-/tmp/svls9604-rc-$(date +%Y%m%d-%H%M%S)}"

mkdir -p "${RESULTS_DIR}"

log() { echo "[$(date -u '+%Y-%m-%dT%H:%M:%SZ')] $*" | tee -a "${RESULTS_DIR}/run.log"; }
header() {
  echo "" | tee -a "${RESULTS_DIR}/run.log"
  echo "==================================================" | tee -a "${RESULTS_DIR}/run.log"
  echo "  $*" | tee -a "${RESULTS_DIR}/run.log"
  echo "==================================================" | tee -a "${RESULTS_DIR}/run.log"
}

# Number of concurrent triggers per stage.
case "${LOAD_STAGE}" in
  L0) CONCURRENCY=1   ;;
  L1) CONCURRENCY=10  ;;
  L2) CONCURRENCY=50  ;;
  L3) CONCURRENCY=100 ;;
  *)  echo "ERROR: unknown LOAD_STAGE ${LOAD_STAGE} (use L0/L1/L2/L3)"; exit 1 ;;
esac

# Azure and GCP URLs.
AZURE_BASE="https://${AZURE_FUNCTION_APP}.azurewebsites.net"
GCP_URL="https://${GCP_REGION}-${GCP_PROJECT}.cloudfunctions.net/${GCP_FUNCTION_NAME}"

header "SVLS-9604 RC Load Test — Stage ${LOAD_STAGE} (${CONCURRENCY} concurrent triggers)"
log "Azure Function : ${AZURE_BASE}/api/httptest"
log "GCP Function   : ${GCP_URL}"
log "DD_SITE        : ${DD_SITE}"
log "Results dir    : ${RESULTS_DIR}"
log "Start time     : $(date -u '+%Y-%m-%dT%H:%M:%SZ')"

TRIGGER_START=$(date -u '+%Y-%m-%dT%H:%M:%SZ')
TRIGGER_EPOCH=$(date +%s)

# ---------------------------------------------------------------------------
# Helper: trigger one function call and record timing.
# Usage: trigger_one <id> <url> <results_file>
# ---------------------------------------------------------------------------
trigger_one() {
  local id="$1" url="$2" out_file="$3"
  local t_start t_end elapsed status

  t_start=$(date +%s%N 2>/dev/null || date +%s)000000000

  # GCP functions need an identity token; Azure uses simple HTTP.
  if [[ "${url}" == *"cloudfunctions"* ]]; then
    local token
    token=$(gcloud auth print-identity-token 2>/dev/null || true)
    if [[ -n "${token}" ]]; then
      status=$(curl -sf -o /dev/null -w "%{http_code}" \
        -H "Authorization: Bearer ${token}" "${url}" 2>/dev/null || echo "000")
    else
      status=$(curl -sf -o /dev/null -w "%{http_code}" "${url}" 2>/dev/null || echo "000")
    fi
  else
    status=$(curl -sf -o /dev/null -w "%{http_code}" "${url}" 2>/dev/null || echo "000")
  fi

  t_end=$(date +%s%N 2>/dev/null || date +%s)000000000
  elapsed=$(( (t_end - t_start) / 1000000 ))  # ms

  echo "${id},${url},${status},${elapsed}" >> "${out_file}"
}

# ---------------------------------------------------------------------------
# Run one wave of concurrent triggers and collect results.
# ---------------------------------------------------------------------------
run_wave() {
  local label="$1" url="$2"
  local wave_file="${RESULTS_DIR}/${label}.csv"

  echo "id,url,http_status,elapsed_ms" > "${wave_file}"

  log "Launching ${CONCURRENCY} concurrent requests → ${label}..."

  local pids=()
  for i in $(seq 1 "${CONCURRENCY}"); do
    trigger_one "${i}" "${url}" "${wave_file}" &
    pids+=($!)
  done
  # Wait for all.
  for pid in "${pids[@]}"; do
    wait "${pid}" 2>/dev/null || true
  done

  # Summarize.
  python3 - "${wave_file}" <<'PYEOF'
import sys, csv, statistics

f = sys.argv[1]
label = f.split('/')[-1].replace('.csv','')
rows = list(csv.DictReader(open(f)))

total   = len(rows)
success = sum(1 for r in rows if r['http_status'].startswith('2'))
fail    = total - success
latencies = [int(r['elapsed_ms']) for r in rows if r['elapsed_ms'].isdigit()]

p50  = statistics.median(latencies) if latencies else 0
p95  = sorted(latencies)[int(len(latencies)*0.95)] if len(latencies) > 1 else (latencies[0] if latencies else 0)
p99  = sorted(latencies)[int(len(latencies)*0.99)] if len(latencies) > 1 else (latencies[0] if latencies else 0)

print(f"  {label}: {total} total | {success} OK | {fail} failed | p50={p50}ms p95={p95}ms p99={p99}ms")
PYEOF
}

# ---------------------------------------------------------------------------
# Stage 1: Azure Function wave
# ---------------------------------------------------------------------------
header "Wave 1 — Azure Function (${CONCURRENCY} concurrent)"
run_wave "azure" "${AZURE_BASE}/api/httptest"

# ---------------------------------------------------------------------------
# Stage 2: GCP Cloud Function wave
# ---------------------------------------------------------------------------
header "Wave 2 — GCP Cloud Function gen1 (${CONCURRENCY} concurrent)"
run_wave "gcp" "${GCP_URL}"

# ---------------------------------------------------------------------------
# Post-trigger metrics check.
# ---------------------------------------------------------------------------
TRIGGER_END=$(date -u '+%Y-%m-%dT%H:%M:%SZ')
TRIGGER_END_EPOCH=$(date +%s)
DURATION=$(( TRIGGER_END_EPOCH - TRIGGER_EPOCH ))

header "Load test complete — ${LOAD_STAGE}"
log "Duration      : ${DURATION}s"
log "Start         : ${TRIGGER_START}"
log "End           : ${TRIGGER_END}"
log "Results       : ${RESULTS_DIR}/"

# ---------------------------------------------------------------------------
# Print EPRW metric queries.
# ---------------------------------------------------------------------------
cat <<METRICS

--- EPRW metrics to check in staging Datadog (datad0g.com) ---

1. Write rate — serverless_compat_agent (should see ${CONCURRENCY}+ accepted):
   sum:event_platform_resource_writer.agentmetadata.serverless_write.accepted{resource_type:serverless_compat_agent}.as_count()

2. Rejection rate (should be 0):
   sum:event_platform_resource_writer.agentmetadata.serverless_rejected.invalid_workload_type{*}.as_count()
   sum:event_platform_resource_writer.agentmetadata.serverless_write.rejected_required_field{*}.as_count()

3. Writes by workload_type:
   sum:event_platform_resource_writer.agentmetadata.serverless_write.accepted{*} by {workload_type}.as_count()

Dashboard: https://app.datad0g.com/dashboard/jer-t39-fee

METRICS

# ---------------------------------------------------------------------------
# Print DDSQL queries for row cardinality validation.
# ---------------------------------------------------------------------------
# api_key_uuid for the staging API key (dd.datad0g.com)
STAGING_API_KEY_UUID=$(python3 -c "
import hashlib, uuid, sys
key = '${DD_API_KEY}'
h = hashlib.md5(key.encode()).digest()
print(str(uuid.UUID(bytes=h)))
" 2>/dev/null || echo "unknown")

cat <<DDSQL
--- DDSQL cardinality validation (run after ~30min) ---

-- 1. Row count should be exactly 2 (one per function), NOT ${CONCURRENCY}:
SELECT workload_type, COUNT(*) as row_count
FROM udm.all.serverless_compat_agent
WHERE api_key_uuid = '${STAGING_API_KEY_UUID}'
GROUP BY workload_type;

-- 2. Full row detail:
SELECT resource_id, resource_name, workload_type,
       serverless_compat_version, serverless_compat_runtime_version,
       region, first_seen_at
FROM udm.all.serverless_compat_agent
WHERE api_key_uuid = '${STAGING_API_KEY_UUID}'
ORDER BY first_seen_at DESC LIMIT 5;

-- 3. Intake lag (time from trigger to first_seen_at):
SELECT resource_id, workload_type,
       first_seen_at,
       TIMESTAMPDIFF(MINUTE, TIMESTAMP '${TRIGGER_START}', first_seen_at) AS lag_minutes
FROM udm.all.serverless_compat_agent
WHERE api_key_uuid = '${STAGING_API_KEY_UUID}'
  AND first_seen_at >= '${TRIGGER_START}'
ORDER BY first_seen_at;

DDSQL

log "Load test done. EPRW metrics should appear within 1-2min."
log "REDAPL rows should appear within 30-60min."
log "Full results saved to: ${RESULTS_DIR}/"
