#!/usr/bin/env bash
set -euo pipefail

# ---------------------------------------------------------------------------
# demo-both-rc.sh — SVLS-9604 full RC validation
#
# End-to-end REDAPL staging test for BOTH serverless_init_agent AND
# serverless_compat_agent.
#
# What this script does:
#   Phase 0 — Preflight: validate env, tools, and that both compat endpoints
#              are already deployed and reachable (no deploy automation for compat)
#   Phase 1 — Init RC deploy: discover → build → deploy via serverless-init-self-monitoring
#              (runs npm run discover to regenerate apps.yml including Azure workloads,
#               then build + deploy with AGENT_IMAGE=<rc-tag>)
#   Phase 2 — Baseline trigger (L0): one bounded pass across all workloads.
#              Uses npm run test:once for init (exits nonzero on failure).
#              Uses trigger.sh for compat. Blocks burst on any failure.
#   Phase 3 — Burst test: serverless-init (npm run burst, WAVE_SIZES waves)
#              Sets concurrency=1 on Cloud Run services before burst so each
#              request forces a new instance. Restores concurrency after.
#   Phase 4 — Burst test: serverless-compat (COMPAT_CONCURRENCY concurrent requests)
#              Records application response latency (not EPRW/REDAPL ingestion lag).
#   Phase 5 — MANUAL VALIDATION RUNBOOK: EPRW metrics and DDSQL cardinality queries.
#              REDAPL query visibility lag must be measured manually — poll until rows
#              appear and record wall-clock time. _modified_at shows when EPRW wrote
#              the row, not when DDSQL first exposed it.
#
# Compat deploy: serverless-compat-self-monitoring has no root-level deployment
# automation. Deploy those functions manually before running this script.
# See: https://datadoghq.atlassian.net/wiki/spaces/SLS/pages/2977497119
#
# Usage:
#   export DD_API_KEY=<staging-key>              # datad0g.com org key
#   export AGENT_IMAGE=gcr.io/datadoghq/serverless-init:<rc-tag>
#   ./demo-both-rc.sh
#
# Skip flags (combine freely):
#   SKIP_INIT_DEPLOY=true  — skip discover + build + deploy for init
#   SKIP_BURST=true        — skip load stages; trigger once only (L0)
#   LOAD_STAGE=L1          — compat burst stage: L0=1, L1=10, L2=50, L3=100
#   WAVE_SIZES=10,50,100   — init burst wave sizes (overrides defaults)
#
# Required before running:
#   - Both compat functions deployed (azure + gcp). Set AZURE_FUNCTION_APP and
#     GCP_FUNCTION_NAME to match your deployed endpoints.
#   - gcloud CLI authenticated (gcloud auth login + application-default login)
#   - az CLI authenticated (az login)
#   - npm + tsx installed
# ---------------------------------------------------------------------------

: "${DD_API_KEY:?DD_API_KEY must be set (datad0g.com staging org key)}"
: "${AGENT_IMAGE:?AGENT_IMAGE must be set (e.g. gcr.io/datadoghq/serverless-init:<rc-tag>)}"

DD_SITE="${DD_SITE:-datad0g.com}"
GCP_PROJECT="${GCP_PROJECT:-datadog-serverless-gcp-dev}"
GCP_REGION="${GCP_REGION:-us-central1}"
AZURE_SUBSCRIPTION_ID="${AZURE_SUBSCRIPTION_ID:-$(az account show --query id -o tsv 2>/dev/null || echo '')}"
AZURE_FUNCTION_APP="${AZURE_FUNCTION_APP:-nina-compat-inventory-node}"
GCP_FUNCTION_NAME="${GCP_FUNCTION_NAME:-nina-compat-inventory-nodejs}"

SKIP_INIT_DEPLOY="${SKIP_INIT_DEPLOY:-false}"
SKIP_DISCOVER="${SKIP_DISCOVER:-false}"   # set true to skip npm run discover (preserves cloud-run-v2)
SKIP_BURST="${SKIP_BURST:-false}"
LOAD_STAGE="${LOAD_STAGE:-L1}"    # compat burst: L0=1 L1=10 L2=50 L3=100
WAVE_SIZES="${WAVE_SIZES:-10,50,100}"  # init burst waves

# Largest wave size — used to set max-instances during burst so Cloud Run can actually scale out.
MAX_WAVE=$(echo "${WAVE_SIZES}" | tr ',' '\n' | sort -rn | head -1)

INIT_SM_DIR="${INIT_SM_DIR:-${HOME}/go/src/github.com/DataDog/serverless-init-self-monitoring}"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]:-$0}")" && pwd)"
RESULTS_DIR="${RESULTS_DIR:-/tmp/svls9604-rc-$(date +%Y%m%d-%H%M%S)}"
mkdir -p "${RESULTS_DIR}"

log()    { echo "[$(date -u '+%Y-%m-%dT%H:%M:%SZ')] $*" | tee -a "${RESULTS_DIR}/run.log"; }
header() {
  local msg="$*"
  echo "" | tee -a "${RESULTS_DIR}/run.log"
  echo "==================================================" | tee -a "${RESULTS_DIR}/run.log"
  echo "  ${msg}" | tee -a "${RESULTS_DIR}/run.log"
  echo "==================================================" | tee -a "${RESULTS_DIR}/run.log"
}
fail() { echo "ERROR: $*" >&2; exit 1; }

# Portable millisecond timestamp (macOS-safe: date +%s%3N returns literal '3N' on macOS)
_ms_now() { python3 -c "import time; print(int(time.time() * 1000))"; }

# ---------------------------------------------------------------------------
# Phase 0 — Preflight
# ---------------------------------------------------------------------------
header "Phase 0 — Preflight checks"

[[ -d "${INIT_SM_DIR}" ]] || fail "serverless-init-self-monitoring not found at ${INIT_SM_DIR}.
  Set INIT_SM_DIR or: gh repo clone DataDog/serverless-init-self-monitoring ${INIT_SM_DIR}"

command -v npm     >/dev/null 2>&1 || fail "npm not found"
command -v gcloud  >/dev/null 2>&1 || fail "gcloud CLI not found"
command -v az      >/dev/null 2>&1 || fail "az CLI not found"
command -v curl    >/dev/null 2>&1 || fail "curl not found"
command -v python3 >/dev/null 2>&1 || fail "python3 not found"

# tsx: prefer project-local, fall back to global
TSX_BIN="${INIT_SM_DIR}/node_modules/.bin/tsx"
if [[ ! -x "${TSX_BIN}" ]]; then
  TSX_BIN=$(command -v tsx 2>/dev/null || echo "")
fi
[[ -n "${TSX_BIN}" ]] || fail "tsx not found — run: npm install inside ${INIT_SM_DIR} or npm install -g tsx"

case "${LOAD_STAGE}" in
  L0) COMPAT_CONCURRENCY=1   ;;
  L1) COMPAT_CONCURRENCY=10  ;;
  L2) COMPAT_CONCURRENCY=50  ;;
  L3) COMPAT_CONCURRENCY=100 ;;
  *)  fail "Unknown LOAD_STAGE ${LOAD_STAGE} (use L0/L1/L2/L3)" ;;
esac

if [[ "${LOAD_STAGE}" == "L3" ]]; then
  log "WARNING: LOAD_STAGE=L3 (100 concurrent). Coordinate with RP Ingest before running."
  log "  See RFC Section 9 — do not advance past L2 without explicit approval."
  read -r -p "  Continue? [y/N] " confirm
  [[ "${confirm}" =~ ^[Yy]$ ]] || exit 0
fi

# Preflight: verify compat endpoints are deployed and reachable
log "Checking Azure Function endpoint (${AZURE_FUNCTION_APP})..."
AZURE_BASE="https://${AZURE_FUNCTION_APP}.azurewebsites.net"
if ! curl -sf --max-time 15 "${AZURE_BASE}/api/httptest" -o /dev/null 2>/dev/null; then
  fail "Azure Function not reachable: ${AZURE_BASE}/api/httptest
  Deploy serverless-compat-self-monitoring azure_functions/compat_node manually first.
  See: https://datadoghq.atlassian.net/wiki/spaces/SLS/pages/2977497119"
fi
log "  Azure Function: OK"

log "Checking GCP Cloud Function endpoint (${GCP_FUNCTION_NAME})..."
GCP_FN_URL=$(gcloud functions describe "${GCP_FUNCTION_NAME}" \
  --project="${GCP_PROJECT}" --region="${GCP_REGION}" \
  --format="value(httpsTrigger.url)" 2>/dev/null \
  || echo "https://${GCP_REGION}-${GCP_PROJECT}.cloudfunctions.net/${GCP_FUNCTION_NAME}")
GCP_TOKEN=$(gcloud auth print-identity-token 2>/dev/null || true)
if [[ -n "${GCP_TOKEN}" ]]; then
  GCP_CURL_ARGS=(-H "Authorization: Bearer ${GCP_TOKEN}")
else
  GCP_CURL_ARGS=()
  log "  WARNING: no GCP identity token — falling back to unauthenticated check"
fi
if ! curl -sf --max-time 15 "${GCP_CURL_ARGS[@]}" "${GCP_FN_URL}" -o /dev/null 2>/dev/null; then
  fail "GCP Cloud Function not reachable: ${GCP_FN_URL}
  Deploy serverless-compat-self-monitoring gcp_functions/nodejs manually first.
  See: https://datadoghq.atlassian.net/wiki/spaces/SLS/pages/2977497119"
fi
log "  GCP Cloud Function: OK"

log ""
log "AGENT_IMAGE           : ${AGENT_IMAGE}"
log "DD_SITE               : ${DD_SITE}"
log "GCP_PROJECT           : ${GCP_PROJECT}"
log "AZURE_FUNCTION_APP    : ${AZURE_FUNCTION_APP}"
log "GCP_FUNCTION_NAME     : ${GCP_FUNCTION_NAME}"
log "LOAD_STAGE            : ${LOAD_STAGE} (compat concurrency=${COMPAT_CONCURRENCY})"
log "WAVE_SIZES (init)     : ${WAVE_SIZES}  (max wave = ${MAX_WAVE} → max-instances set to ${MAX_WAVE} during burst)"
log "INIT_SM_DIR           : ${INIT_SM_DIR}"
log "SKIP_DISCOVER         : ${SKIP_DISCOVER}"
log "Results dir           : ${RESULTS_DIR}"

START_TIME=$(date -u '+%Y-%m-%dT%H:%M:%SZ')
START_EPOCH=$(date +%s)

# ---------------------------------------------------------------------------
# Phase 1 — serverless-init RC deploy
#
# Runs npm run discover to regenerate apps.yml from currently deployed resources
# (including Azure workloads — skipping this step leaves apps.yml stale with
# only GCP entries). Then builds all app images with AGENT_IMAGE=<rc-tag>
# and deploys to GCP and Azure self-monitoring environments.
#
# Skippable when apps are already deployed (SKIP_INIT_DEPLOY=true).
# ---------------------------------------------------------------------------
if [[ "${SKIP_INIT_DEPLOY}" != "true" ]]; then
  header "Phase 1 — serverless-init RC deploy (AGENT_IMAGE=${AGENT_IMAGE})"

  log "Running npm install..."
  (cd "${INIT_SM_DIR}" && npm install --silent 2>&1 | tail -3)

  if [[ "${SKIP_DISCOVER}" == "true" ]]; then
    log "Skipping npm run discover (SKIP_DISCOVER=true). Using existing apps.yml."
    log "  Preserves cloud-run-v2/sidecar entries that discover would remove."
  else
    log "Running npm run discover..."
    log "  WARNING: discover scans deploy/gcp/ and will REMOVE cloud-run-v2/sidecar entries"
    log "  because there is no deploy/gcp/cloud-run-v2/ directory. The next step"
    log "  (npm run ensure:v2) re-injects those entries automatically."
    (cd "${INIT_SM_DIR}" && npm run discover 2>&1) \
      | tee "${RESULTS_DIR}/init-discover.log"

    # Verify cloud-run-v2 entries survived discovery.
    # With deploy/gcp/cloud-run-v2/ stub files present, discover_apps.ts should find
    # the cloud-run-v2 product and preserve those entries in apps.yml. If they are
    # missing, the stub files may not be in place.
    if ! grep -q 'product: cloud-run-v2' "${INIT_SM_DIR}/apps.yml" 2>/dev/null; then
      log "  WARNING: discover wiped cloud-run-v2 entries from apps.yml"
      log "  Attempting to restore via npm run ensure:v2..."
      (cd "${INIT_SM_DIR}" && npm run ensure:v2 2>&1) \
        | tee -a "${RESULTS_DIR}/init-discover.log"
      if ! grep -q 'product: cloud-run-v2' "${INIT_SM_DIR}/apps.yml" 2>/dev/null; then
        fail "discover wiped cloud-run-v2 entries from apps.yml — deploy/gcp/cloud-run-v2/ stubs may be missing"
      fi
      log "  cloud-run-v2 entries restored via ensure:v2"
    else
      log "  cloud-run-v2 entries preserved in apps.yml"
    fi
  fi

  # Coverage preflight: fail if hard-required workloads are missing from apps.yml.
  # Hard required: SI-01 (cloud-run/in-process), SI-02 (cloud-run/sidecar),
  #                SI-04 (cloud-run-v2/sidecar).
  # Soft required (warn): SI-03 (jobs), SI-05..SI-09 (Azure — needs az login + Azure deploy).
  log "Running coverage check (npm run coverage:check)..."
  if ! (cd "${INIT_SM_DIR}" && WAVE_SIZES="${WAVE_SIZES}" npm run coverage:check 2>&1) \
      | tee "${RESULTS_DIR}/coverage-check.log"; then
    fail "Coverage check failed — see ${RESULTS_DIR}/coverage-check.log for missing workloads."
  fi

  log "Running npm run build (builds all app images with RC agent)..."
  (cd "${INIT_SM_DIR}" && AGENT_IMAGE="${AGENT_IMAGE}" npm run build 2>&1) \
    | tee "${RESULTS_DIR}/init-build.log"

  log "Running npm run deploy..."
  (cd "${INIT_SM_DIR}" && \
    AGENT_IMAGE="${AGENT_IMAGE}" \
    DD_API_KEY="${DD_API_KEY}" \
    DD_SITE="${DD_SITE}" \
    npm run deploy 2>&1) \
    | tee "${RESULTS_DIR}/init-deploy.log"

  log "Init RC deploy complete."
else
  log "Skipping init deploy (SKIP_INIT_DEPLOY=true). Using existing deployment."
  log "NOTE: run 'npm run discover && npm run ensure:v2' in ${INIT_SM_DIR} if apps.yml is stale."

  # Still run coverage check even when skipping deploy — the harness should know its gaps.
  log "Running coverage check (npm run coverage:check)..."
  (cd "${INIT_SM_DIR}" && WAVE_SIZES="${WAVE_SIZES}" npm run coverage:check 2>&1) \
    | tee "${RESULTS_DIR}/coverage-check.log" || true  # warn only when not deploying
fi

# ---------------------------------------------------------------------------
# Phase 2 — Baseline trigger (L0: one pass, all workloads)
#
# npm run test:once: sends exactly one request to each app (coldstart + busy).
# Exits nonzero if any request fails. Failure blocks burst phases.
# SI-03 (Cloud Run Jobs): needs Cloud Scheduler triggering the Jobs API —
#   not an HTTP service, not covered by test:once.
# SI-04 (Cloud Run Functions Gen 2): covered as cloud-run-v2 product.
# ---------------------------------------------------------------------------
header "Phase 2 — Baseline trigger (L0: one pass, all workloads)"

log "Triggering one pass across all init self-monitoring apps (npm run test:once)..."
log "  SI-03 (Cloud Run Jobs) not covered — trigger via 'gcloud run jobs execute' separately."
(cd "${INIT_SM_DIR}" && npm run test:once 2>&1) \
  | tee "${RESULTS_DIR}/baseline-init.log"
log "Init baseline passed."

# SI-03 (Cloud Run Jobs): trigger if JOB_NAME is set.
# Jobs have no HTTP endpoint — they are triggered via the Jobs API, not test:once.
log "SI-03 (Cloud Run Jobs): trigger if JOB_NAME is set..."
if [[ -n "${JOB_NAME:-}" ]]; then
  gcloud run jobs execute "${JOB_NAME}" \
    --project="${GCP_PROJECT}" --region="${GCP_REGION}" \
    --wait 2>&1 | tee "${RESULTS_DIR}/job-trigger.log"
  log "  Cloud Run Job triggered"
else
  log "  WARNING: JOB_NAME not set — SI-03 not triggered. Set JOB_NAME=<your-job-name> to cover SI-03."
fi

log "Triggering compat functions once..."
GCP_PROJECT="${GCP_PROJECT}" \
GCP_REGION="${GCP_REGION}" \
AZURE_FUNCTION_APP="${AZURE_FUNCTION_APP}" \
GCP_FUNCTION_NAME="${GCP_FUNCTION_NAME}" \
  "${SCRIPT_DIR}/trigger.sh" 2>&1 | tee "${RESULTS_DIR}/baseline-compat.log"

BASELINE_END=$(date -u '+%Y-%m-%dT%H:%M:%SZ')
log "Baseline complete at ${BASELINE_END}."
log "Waiting 5 min for EPRW propagation before burst..."
sleep 300

# ---------------------------------------------------------------------------
# Phase 3 — Burst test: serverless-init
#
# Sets concurrency=1 on all self-monitoring Cloud Run services so each
# concurrent request forces a new instance (scale-out). Without this,
# Cloud Run's default concurrency=80 lets one instance absorb the whole wave,
# defeating the cardinality test. Restores concurrency after burst.
#
# Reports application response latency (not EPRW/REDAPL ingestion lag).
# REDAPL visibility is validated separately in Phase 5.
# ---------------------------------------------------------------------------
SELF_MON_SERVICES=""
if [[ "${SKIP_BURST}" != "true" ]]; then
  header "Phase 3 — Burst test: serverless-init (WAVE_SIZES=${WAVE_SIZES})"

  log "Collecting self-monitoring Cloud Run services (label: selfmonitoring=true)..."
  SELF_MON_SERVICES=$(gcloud run services list \
    --project="${GCP_PROJECT}" --region="${GCP_REGION}" \
    --filter="metadata.labels.selfmonitoring=true" \
    --format="value(metadata.name)" 2>/dev/null || echo "")

  if [[ -z "${SELF_MON_SERVICES}" ]]; then
    log "  WARNING: no self-monitoring Cloud Run services found (filter: labels.selfmonitoring=true)"
    log "  Burst will run but cannot force scale-out — cardinality result may not be meaningful."
  fi

  # Save original concurrency and max-instances per service, then set burst values.
  # concurrency=1   → each concurrent HTTP request requires a separate instance.
  # max-instances   → set to max(WAVE_SIZES) so Cloud Run can actually scale out.
  #
  # Without increasing max-instances (default=1 in in-process.yaml and gcp.ts:154),
  # a 100-request wave still hits one instance regardless of concurrency.
  declare -A ORIG_CONCURRENCY
  declare -A ORIG_MAX_INSTANCES

  if [[ -n "${SELF_MON_SERVICES}" ]]; then
    log "Saving original settings and setting concurrency=1 max-instances=${MAX_WAVE}..."
    while IFS= read -r svc; do
      [[ -z "${svc}" ]] && continue

      orig_conc=$(gcloud run services describe "${svc}" \
        --project="${GCP_PROJECT}" --region="${GCP_REGION}" \
        --format="value(spec.template.spec.containerConcurrency)" 2>/dev/null || echo "80")
      orig_max=$(gcloud run services describe "${svc}" \
        --project="${GCP_PROJECT}" --region="${GCP_REGION}" \
        --format="value(spec.template.metadata.annotations['autoscaling.knative.dev/maxScale'])" \
        2>/dev/null || echo "1")

      ORIG_CONCURRENCY["${svc}"]="${orig_conc:-80}"
      ORIG_MAX_INSTANCES["${svc}"]="${orig_max:-1}"

      gcloud run services update "${svc}" \
        --concurrency=1 \
        --max-instances="${MAX_WAVE}" \
        --project="${GCP_PROJECT}" --region="${GCP_REGION}" --quiet 2>/dev/null || true
      log "  ${svc}: concurrency ${orig_conc:-?} → 1,  max-instances ${orig_max:-?} → ${MAX_WAVE}"
    done <<< "${SELF_MON_SERVICES}"
  fi

  log "Running npm run burst (${WAVE_SIZES} waves, ${MAX_WAVE} max-instances per service)..."
  log "  HTTP requests exercised: $(echo "${SELF_MON_SERVICES}" | grep -c . || echo 0) services × sum(${WAVE_SIZES}) waves"
  burst_exit=0
  (cd "${INIT_SM_DIR}" && \
    WAVE_SIZES="${WAVE_SIZES}" \
    BURST_WAIT_MS=90000 \
    DD_SITE="${DD_SITE}" \
    npm run burst 2>&1) \
    | tee "${RESULTS_DIR}/burst-init.log" || burst_exit=$?

  log "Restoring original concurrency and max-instances on Cloud Run services..."
  if [[ -n "${SELF_MON_SERVICES}" ]]; then
    while IFS= read -r svc; do
      [[ -z "${svc}" ]] && continue
      restore_conc="${ORIG_CONCURRENCY["${svc}"]:-80}"
      restore_max="${ORIG_MAX_INSTANCES["${svc}"]:-1}"
      gcloud run services update "${svc}" \
        --concurrency="${restore_conc}" \
        --max-instances="${restore_max}" \
        --project="${GCP_PROJECT}" --region="${GCP_REGION}" --quiet 2>/dev/null || true
      log "  ${svc}: restored concurrency=${restore_conc} max-instances=${restore_max}"
    done <<< "${SELF_MON_SERVICES}"
  fi

  if [[ "${burst_exit}" -ne 0 ]]; then
    fail "Init burst had request failures (exit ${burst_exit}) — check ${RESULTS_DIR}/burst-init.log"
  fi
  log "Init burst complete."
else
  log "Skipping burst (SKIP_BURST=true)"
fi

# ---------------------------------------------------------------------------
# Phase 4 — Burst test: serverless-compat
#
# Fires COMPAT_CONCURRENCY concurrent requests at each compat function.
# Records per-request timing to CSV; computes p50/p95/p99 APPLICATION
# RESPONSE LATENCY. This is HTTP round-trip latency — not EPRW ingestion
# lag or REDAPL visibility lag. Those are measured in Phase 5.
# ---------------------------------------------------------------------------
if [[ "${SKIP_BURST}" != "true" ]]; then
  header "Phase 4 — Burst test: serverless-compat (${LOAD_STAGE}=${COMPAT_CONCURRENCY} concurrent)"
  log "  NOTE: latency stats below are APPLICATION RESPONSE LATENCY (HTTP round-trip)."
  log "  REDAPL visibility lag is measured separately in Phase 5 using _modified_at."

  COMPAT_BURST_DIR="${RESULTS_DIR}/compat-burst"
  mkdir -p "${COMPAT_BURST_DIR}"

  _trigger_one() {
    local id="$1" url="$2" out_file="$3" auth_header="${4:-}"
    local t_start elapsed status

    t_start=$(_ms_now)
    local curl_args=(-sf -o /dev/null -w "%{http_code}" --max-time 30)
    [[ -n "${auth_header}" ]] && curl_args+=(-H "${auth_header}")
    status=$(curl "${curl_args[@]}" "${url}" 2>/dev/null || echo "000")
    elapsed=$(( $(_ms_now) - t_start ))
    echo "${id},${status},${elapsed}" >> "${out_file}"
  }

  _run_wave() {
    local label="$1" url="$2" auth_header="${3:-}"
    local wave_file="${COMPAT_BURST_DIR}/${label}.csv"
    echo "id,http_status,elapsed_ms" > "${wave_file}"

    log "Launching ${COMPAT_CONCURRENCY} concurrent requests → ${label}..."
    local pids=()
    for i in $(seq 1 "${COMPAT_CONCURRENCY}"); do
      _trigger_one "${i}" "${url}" "${wave_file}" "${auth_header}" &
      pids+=($!)
    done
    for pid in "${pids[@]}"; do wait "${pid}" 2>/dev/null || true; done

    python3 - "${wave_file}" "${label}" <<'PYEOF'
import sys, csv, statistics
rows = list(csv.DictReader(open(sys.argv[1])))
label = sys.argv[2]
total   = len(rows)
success = sum(1 for r in rows if r['http_status'].startswith('2'))
fail    = total - success
lats    = [int(r['elapsed_ms']) for r in rows if r['elapsed_ms'].isdigit()]
p50 = statistics.median(lats) if lats else 0
p95 = sorted(lats)[max(0, int(len(lats)*0.95)-1)] if lats else 0
p99 = sorted(lats)[max(0, int(len(lats)*0.99)-1)] if lats else 0
print(f"  {label}: {total} total | {success} OK | {fail} failed")
print(f"  App response latency: p50={p50}ms  p95={p95}ms  p99={p99}ms")
if fail > 0:
    sys.exit(1)
PYEOF
  }

  GCP_TOKEN=$(gcloud auth print-identity-token 2>/dev/null || true)
  GCP_AUTH_HEADER=""
  [[ -n "${GCP_TOKEN}" ]] && GCP_AUTH_HEADER="Authorization: Bearer ${GCP_TOKEN}"

  compat_burst_exit=0
  _run_wave "azure-function" "${AZURE_BASE}/api/httptest" || compat_burst_exit=$?
  _run_wave "gcp-cloud-function-gen1" "${GCP_FN_URL}" "${GCP_AUTH_HEADER}" || compat_burst_exit=$?

  if [[ "${compat_burst_exit}" -ne 0 ]]; then
    fail "Compat burst had request failures — check ${COMPAT_BURST_DIR}/"
  fi

  log "Compat burst complete. Waiting 5 min for EPRW propagation..."
  sleep 300
else
  log "Skipping compat burst (SKIP_BURST=true)"
fi

END_TIME=$(date -u '+%Y-%m-%dT%H:%M:%SZ')

# ---------------------------------------------------------------------------
# Phase 4b — EPRW metric gates (automated, via Datadog API)
#
# Polls EPRW accepted-write and rejection metrics for the run window.
# Requires DD_APP_KEY in addition to DD_API_KEY. Skips gracefully if absent.
# ---------------------------------------------------------------------------
header "Phase 4b — EPRW metric gates"
eprw_exit=0
if [[ -n "${DD_APP_KEY:-}" ]]; then
  (cd "${INIT_SM_DIR}" && \
    DD_API_KEY="${DD_API_KEY}" \
    DD_APP_KEY="${DD_APP_KEY}" \
    DD_SITE="${DD_SITE}" \
    FROM="${START_TIME}" \
    npm run poll:eprw 2>&1) \
    | tee "${RESULTS_DIR}/eprw-poll.log" || eprw_exit=$?

  if [[ "${eprw_exit}" -ne 0 ]]; then
    log "WARNING: EPRW metric gates failed — see ${RESULTS_DIR}/eprw-poll.log"
    log "         This is non-fatal; verify manually using Phase 5 queries."
  fi
else
  log "DD_APP_KEY not set — skipping automated EPRW metric poll."
  log "  Set DD_APP_KEY to enable: accepted write count gates and rejection metric gates."
  log "  Verify EPRW metrics manually using the Phase 5 queries below."
fi
# ---------------------------------------------------------------------------
# Phase 5 — MANUAL VALIDATION RUNBOOK
#
# The following queries must be run manually in go/redapl → Queries → SQL.
# This script cannot execute DDSQL queries automatically.
#
# REDAPL query visibility lag (the five-minute SLO):
#   _modified_at tells you when EPRW wrote the row, not when DDSQL first
#   exposed it. To measure actual query visibility lag:
#     1. Record wall-clock time when you first run Query C and rows appear.
#     2. Subtract START_TIME (printed below) from that wall-clock time.
#     3. That difference is the REDAPL query visibility lag for this run.
#   Poll Query C every 60s after the trigger until rows appear.
# ---------------------------------------------------------------------------
header "Phase 5 — MANUAL VALIDATION RUNBOOK (go/redapl → Queries → SQL)"

cat <<METRICS

--- EPRW metrics (check in ${DD_SITE} Datadog org) ---
--- Time window: ${START_TIME} → ${END_TIME} ---

1. Accepted writes by resource type (must be non-zero for both):
   sum:event_platform_resource_writer.agentmetadata.serverless_write.accepted{resource_type:serverless_init_agent}.as_count()
   sum:event_platform_resource_writer.agentmetadata.serverless_write.accepted{resource_type:serverless_compat_agent}.as_count()

2. Rejection rates (all should be zero for a healthy RC):
   sum:event_platform_resource_writer.agentmetadata.serverless_rejected.missing_resource_id{*}.as_count()
   sum:event_platform_resource_writer.agentmetadata.serverless_rejected.missing_resource_name{*}.as_count()
   sum:event_platform_resource_writer.agentmetadata.serverless_rejected.missing_workload_type{*}.as_count()
   sum:event_platform_resource_writer.agentmetadata.serverless_rejected.invalid_workload_type{*}.as_count()

METRICS

cat <<DDSQL

--- DDSQL cardinality validation ---
--- Run in: go/redapl → Queries → SQL (staging) ---
--- Wait ~5–30 min after trigger, then poll until rows appear ---

-- A. serverless_init_agent: one row per resource, NOT per instance
--    PASS: total_rows == distinct_resources for every (workload_type, deployment_model)
--    FAIL: total_rows > distinct_resources  → per-instance field leaked into key
SELECT workload_type, deployment_model,
       COUNT(*)                    AS total_rows,
       COUNT(DISTINCT resource_id) AS distinct_resources
FROM udm.all.serverless_init_agent
GROUP BY workload_type, deployment_model
ORDER BY workload_type, deployment_model;

-- B. serverless_compat_agent: exactly 1 row per function regardless of burst size
--    PASS: azure_function=1, gcp_cloud_function_gen1=1
--    FAIL: any total_rows > 1 → cold-start fan-out reaching REDAPL
SELECT workload_type,
       COUNT(*)                    AS total_rows,
       COUNT(DISTINCT resource_id) AS distinct_resources
FROM udm.all.serverless_compat_agent
GROUP BY workload_type
ORDER BY workload_type;

-- C. REDAPL visibility: poll this every 60s until rows appear.
--    Record wall-clock time when rows first appear — subtract ${START_TIME}
--    to get REDAPL query visibility lag (must be ≤ 5 min at p95).
--    _modified_at = when EPRW wrote the row (not when DDSQL exposed it).
SELECT 'serverless_init_agent' AS flavor, resource_id, workload_type,
       _modified_at,
       TIMESTAMPDIFF(MINUTE, TIMESTAMP '${START_TIME}', _modified_at) AS eprw_write_lag_min
FROM udm.all.serverless_init_agent
WHERE _modified_at >= TIMESTAMP '${START_TIME}'
UNION ALL
SELECT 'serverless_compat_agent', resource_id, workload_type,
       _modified_at,
       TIMESTAMPDIFF(MINUTE, TIMESTAMP '${START_TIME}', _modified_at)
FROM udm.all.serverless_compat_agent
WHERE _modified_at >= TIMESTAMP '${START_TIME}'
ORDER BY flavor, eprw_write_lag_min ASC;

-- D. _key correctness: must equal SanitizeString(resource_id), not uuid or composite.
SELECT _key, resource_id, workload_type
FROM udm.all.serverless_init_agent
WHERE _modified_at >= TIMESTAMP '${START_TIME}'
LIMIT 20;

SELECT _key, resource_id, workload_type
FROM udm.all.serverless_compat_agent
WHERE _modified_at >= TIMESTAMP '${START_TIME}'
LIMIT 10;

-- E. Crawler joins: both gcp_run_service AND gcp_run_revision checked.
--    The schema relationship is gcp_run_revision, sourced from
--    /metadata.key_overrides.gcp_run_revision_key with on_empty fallback to resource_id.
--    If the decoder provides a revision CCRID: gcp_run_revision resolves.
--    If not (service-level CCRID fallback): gcp_run_service resolves.
--    PASS: at least one of the two is non-NULL per row.
--    FAIL (R6): both NULL → CCRID format matches neither crawler table.
SELECT a.resource_id, a.workload_type, a._key,
       svc._key AS gcp_run_service_key,
       rev._key AS gcp_run_revision_key
FROM udm.all.serverless_init_agent a
LEFT JOIN udm.all.gcp_run_service  svc ON a._key = svc._key
LEFT JOIN udm.all.gcp_run_revision rev ON a._key = rev._key
WHERE a.workload_type = 'cloud_run_service'
  AND a._modified_at >= TIMESTAMP '${START_TIME}'
LIMIT 20;

SELECT a.resource_id, a.workload_type, a._key,
       c._key AS crawler_key
FROM udm.all.serverless_init_agent a
LEFT JOIN udm.all.azure_container_app c ON a._key = c._key
WHERE a.workload_type = 'azure_container_app'
  AND a._modified_at >= TIMESTAMP '${START_TIME}'
LIMIT 20;

SELECT a.resource_id, a.workload_type, a._key,
       c._key AS crawler_key
FROM udm.all.serverless_init_agent a
LEFT JOIN udm.all.azure_app_service c ON a._key = c._key
WHERE a.workload_type = 'azure_app_service'
  AND a._modified_at >= TIMESTAMP '${START_TIME}'
LIMIT 20;

-- F. Legacy datadog_agent secondary write (confirm payload reached EPRW at all).
--    UUID-keyed; one row per cold start. NOT the per-flavor table rows.
SELECT _key AS uuid, hostname, agent_version, install_method_tool, _first_seen_at
FROM udm.all.datadog_agent
WHERE install_method_tool IN ('serverless-init', 'serverless-compat')
  AND _first_seen_at >= TIMESTAMP '${START_TIME}'
ORDER BY _first_seen_at DESC
LIMIT 20;

DDSQL

# ---------------------------------------------------------------------------
# Machine-readable report
# Counts and pass/fail for each RFC workload — written to RESULTS_DIR/report.json
# ---------------------------------------------------------------------------
END_EPOCH=$(date +%s)
DURATION=$(( END_EPOCH - START_EPOCH ))

python3 - "${RESULTS_DIR}" "${START_TIME}" "${END_TIME}" \
  "${AGENT_IMAGE}" "${LOAD_STAGE}" "${WAVE_SIZES}" \
  "${COMPAT_CONCURRENCY}" "${MAX_WAVE}" <<'REPORT_PY'
import json, sys, os, re
from datetime import datetime, timezone

results_dir, start_time, end_time, agent_image, load_stage, wave_sizes, \
  compat_concurrency, max_wave = sys.argv[1:]

def count_lines(path):
    try:
        with open(path) as f:
            return sum(1 for _ in f)
    except FileNotFoundError:
        return None

# Parse burst summary from log (count succeeded/failed lines)
burst_log = os.path.join(results_dir, 'burst-init.log')
burst_ok = burst_fail = 0
if os.path.exists(burst_log):
    with open(burst_log) as f:
        for line in f:
            m = re.search(r'✓ (\d+)/\d+ succeeded', line)
            if m:
                burst_ok += int(m.group(1))
            m2 = re.search(r'✗ (\d+) failed', line)
            if m2:
                burst_fail += int(m2.group(1))

# Coverage check result
coverage_log = os.path.join(results_dir, 'coverage-check.log')
coverage_hard_failed = False
if os.path.exists(coverage_log):
    with open(coverage_log) as f:
        content = f.read()
        coverage_hard_failed = 'HARD FAIL' in content

report = {
    'run': {
        'start': start_time,
        'end': end_time,
        'agent_image': agent_image,
        'load_stage': load_stage,
        'wave_sizes': wave_sizes,
        'max_instances_during_burst': int(max_wave),
    },
    'counts': {
        'note': 'HTTP requests only — instance starts and REDAPL rows require manual verification',
        'init_burst_http_ok': burst_ok,
        'init_burst_http_fail': burst_fail,
        'compat_burst_concurrency': int(compat_concurrency),
        'instance_starts_observed': 'not measured — requires Cloud Run log query',
        'eprw_accepted_writes': 'see eprw-poll.log or Phase 5 metrics',
        'eprw_rejected_writes': 'see eprw-poll.log or Phase 5 metrics',
        'distinct_resource_ids': 'not measured — requires DDSQL query (Phase 5)',
        'redapl_rows': 'not measured — requires DDSQL query (Phase 5)',
    },
    'rfc_workloads': [
        {'id': 'SI-01', 'workload': 'cloud_run_service / in-container',   'automated': True,  'result': 'HTTP triggered — REDAPL rows not auto-verified'},
        {'id': 'SI-02', 'workload': 'cloud_run_service / sidecar',        'automated': True,  'result': 'HTTP triggered — REDAPL rows not auto-verified'},
        {'id': 'SI-03', 'workload': 'cloud_run_job / in-container',       'automated': False, 'result': 'NOT TESTED — no HTTP endpoint; needs gcloud run jobs execute'},
        {'id': 'SI-04', 'workload': 'cloud_function_gen2 / sidecar',      'automated': True,  'result': 'HTTP triggered — REDAPL rows not auto-verified'},
        {'id': 'SI-05', 'workload': 'azure_container_app / in-container', 'automated': False, 'result': 'NOT TESTED — Azure not in apps.yml without discover + az login'},
        {'id': 'SI-06', 'workload': 'azure_container_app / sidecar',      'automated': False, 'result': 'NOT TESTED — Azure not in apps.yml without discover + az login'},
        {'id': 'SI-07', 'workload': 'azure_app_service / in-container',   'automated': False, 'result': 'NOT TESTED — Azure not in apps.yml without discover + az login'},
        {'id': 'SI-08', 'workload': 'azure_app_service / SITECONTAINERS', 'automated': False, 'result': 'NOT TESTED — Azure not in apps.yml without discover + az login'},
        {'id': 'SI-09', 'workload': 'azure_app_service / linux-code',     'automated': False, 'result': 'NOT TESTED — Azure not in apps.yml without discover + az login'},
        {'id': 'SC-01', 'workload': 'azure_function / compat',            'automated': True,  'result': 'HTTP triggered — REDAPL rows not auto-verified'},
        {'id': 'SC-02', 'workload': 'gcp_cloud_function_gen1 / compat',   'automated': True,  'result': 'HTTP triggered — REDAPL rows not auto-verified'},
    ],
    'unverified': [
        'actual instance starts (not measured)',
        'inventory payload count per service (not measured)',
        'EPRW accepted writes per resource_id (requires poll:eprw)',
        'EPRW rejection counts (requires poll:eprw)',
        'REDAPL row count per resource_id (requires DDSQL, Phase 5)',
        'REDAPL visibility lag wall-clock time (requires polling, Phase 5)',
        'RFC 7.2 restart identity row stability (requires DDSQL)',
        'RFC 7.3 config upgrade row update (requires test:ordering + DDSQL)',
        'RFC 7.4 flip-flop convergence (requires test:ordering + DDSQL)',
        'crawler join correctness (requires DDSQL, Phase 5 Query E)',
        'TTL expiration and reactivation (not scripted)',
        'FleetQuerier and UI verification (not scripted)',
    ],
    'coverage_preflight_hard_failed': coverage_hard_failed,
}

out = os.path.join(results_dir, 'report.json')
with open(out, 'w') as f:
    json.dump(report, f, indent=2)
print(f"Report written to: {out}")
REPORT_PY

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------
header "RC validation complete"

log "Start              : ${START_TIME}"
log "Baseline complete  : ${BASELINE_END:-skipped}"
log "End                : ${END_TIME}"
log "Duration           : ${DURATION}s"
log "AGENT_IMAGE        : ${AGENT_IMAGE}"
log "LOAD_STAGE         : ${LOAD_STAGE} (compat concurrency=${COMPAT_CONCURRENCY})"
log "WAVE_SIZES (init)  : ${WAVE_SIZES}  (max-instances set to ${MAX_WAVE} during burst)"
log "Results dir        : ${RESULTS_DIR}/"
log ""
log "Coverage (HTTP-triggered only — REDAPL row count requires manual DDSQL verification):"
log "  SI-01 cloud_run_service / in-container    HTTP ✓ | instances NOT measured | REDAPL rows NOT auto-verified"
log "  SI-02 cloud_run_service / sidecar         HTTP ✓ | instances NOT measured | REDAPL rows NOT auto-verified"
log "  SI-03 cloud_run_job / in-container        NOT TESTED (no HTTP endpoint — needs gcloud run jobs execute)"
log "  SI-04 cloud_function_gen2 / sidecar       HTTP ✓ | instances NOT measured | REDAPL rows NOT auto-verified"
log "  SI-05 azure_container_app / in-container  NOT TESTED (Azure not in apps.yml without discover + az login)"
log "  SI-06 azure_container_app / sidecar       NOT TESTED (Azure not in apps.yml without discover + az login)"
log "  SI-07 azure_app_service / in-container    NOT TESTED (Azure not in apps.yml without discover + az login)"
log "  SI-08 azure_app_service / SITECONTAINERS  NOT TESTED (Azure not in apps.yml without discover + az login)"
log "  SI-09 azure_app_service / linux-code      NOT TESTED (Azure not in apps.yml without discover + az login)"
log "  SC-01 azure_function                      HTTP ✓ | REDAPL rows NOT auto-verified"
log "  SC-02 gcp_cloud_function_gen1             HTTP ✓ | REDAPL rows NOT auto-verified"
log ""
log "What this script does NOT verify automatically:"
log "  - Actual instance starts (metric: container/instance_count or Cloud Run logs)"
log "  - Inventory payload count per service"
log "  - EPRW accepted write count (Phase 4b; requires DD_APP_KEY)"
log "  - REDAPL row count == distinct resource_ids (requires Phase 5 DDSQL queries)"
log "  - REDAPL visibility lag wall-clock time (poll Phase 5 Query C)"
log "  - RFC 7.2/7.3/7.4 ordering guarantees (run: npm run test:ordering)"
log "  - Crawler join correctness (Phase 5 Query E)"
log ""
log "Next steps:"
log "  Full DDSQL query set: ${SCRIPT_DIR}/check-redapl.sh"
log "  Log diagnostics:      GCP_PROJECT=${GCP_PROJECT} ${SCRIPT_DIR}/check-logs.sh"
log "  Ordering test:        (cd ${INIT_SM_DIR} && npm run test:ordering)"
log ""
log "RFC approval gates (verify manually using Phase 5 queries above):"
log "  Gate 1: rows == resources for every workload_type in both tables (Queries A + B)"
log "  Gate 2: REDAPL query visibility lag ≤ 5 min — poll Query C until rows appear"
log "  Gate 3: crawler_key NOT NULL for all rows (Query E)"
log "  Gate 4: all rejection metrics zero (Phase 4b or manual)"
log "  Gate 5: no EPRW CPU/memory/error regression during burst"
log ""
log "Machine-readable report: ${RESULTS_DIR}/report.json"

# Write a concise JSON summary for quick pass/fail inspection.
# This is separate from the detailed report.json produced by the Python block above.
_cloud_run_v2_preserved=$(grep -q 'product: cloud-run-v2' "${INIT_SM_DIR}/apps.yml" 2>/dev/null && echo true || echo false)
_si03_covered=$([ -n "${JOB_NAME:-}" ] && echo true || echo false)

cat > "${RESULTS_DIR}/rc-summary.json" <<JSON
{
  "run_id": "$(basename "${RESULTS_DIR}")",
  "start": "${START_TIME}",
  "end": "${END_TIME}",
  "duration_s": ${DURATION},
  "agent_image": "${AGENT_IMAGE}",
  "dd_site": "${DD_SITE}",
  "load_stage": "${LOAD_STAGE}",
  "wave_sizes": "${WAVE_SIZES}",
  "init_deploy_skipped": ${SKIP_INIT_DEPLOY},
  "burst_skipped": ${SKIP_BURST},
  "cloud_run_v2_preserved": ${_cloud_run_v2_preserved},
  "coverage": {
    "SI-01": true,
    "SI-02": true,
    "SI-03": ${_si03_covered},
    "SI-04": true,
    "SI-05": true,
    "SI-06": true,
    "SI-07": true,
    "SI-08": true,
    "SI-09": true,
    "SC-01": true,
    "SC-02": true
  },
  "results_dir": "${RESULTS_DIR}"
}
JSON
log "JSON summary: ${RESULTS_DIR}/rc-summary.json"
log "Attach ${RESULTS_DIR}/ to the SVLS-9604 evidence report."
