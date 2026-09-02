#!/usr/bin/env bash
set -euo pipefail

# ---------------------------------------------------------------------------
# check-logs.sh — diagnostic log collector for SVLS-9604
#
# Collects inventory diagnostic output from deployed self-monitoring services
# for both serverless-init and serverless-compat, then prints the
# resource_id-based DDSQL queries to validate rows in both REDAPL tables.
#
# Resource identity: both REDAPL tables key on resource_id (CCRID), not UUID.
# Look for log lines containing "resource_id=" to find the actual key.
#
# Required env vars:
#   GCP_PROJECT            — GCP project for init Cloud Run services
#   AZURE_SUBSCRIPTION_ID  — Azure subscription (auto-detected if omitted)
#
# Service name overrides (defaults match the POC deploy scripts):
#   INIT_CR_SERVICE    — Cloud Run service name for init in-container  (SI-01)
#   INIT_CR_SIDECAR    — Cloud Run service name for init sidecar        (SI-02)
#   INIT_CR_JOB        — Cloud Run job name                             (SI-03, separate POC)
#   INIT_CR_FN_SIDECAR — Cloud Run service name for Functions Gen 2     (SI-04, separate POC)
#   INIT_ACA_INIT      — Azure Container App name, in-container         (SI-05)
#   INIT_ACA_SIDECAR   — Azure Container App name, sidecar              (SI-06)
#   INIT_AAS_CONTAINER — Azure App Service, Linux container             (SI-07)
#   INIT_AAS_SIDECAR   — Azure App Service, SITECONTAINERS              (SI-08)
#   INIT_AAS_CODE      — Azure App Service, Linux code                  (SI-09)
#   COMPAT_AZURE_APP   — Azure Function App name                        (SC-01)
#   COMPAT_GCP_FN      — GCP Cloud Function Gen 1 name                  (SC-02)
#   COMPAT_GCP_PROJECT — GCP project for compat functions (may differ from init)
#   AZURE_RG_ACA       — Resource group for Container Apps
#   AZURE_RG_AAS       — Resource group for App Service apps
#   COMPAT_AZURE_RG    — Resource group for compat Azure Function
#
# Output: diagnostic-results-YYYYMMDD-HHMMSS.txt
# ---------------------------------------------------------------------------

: "${GCP_PROJECT:?GCP_PROJECT must be set}"
AZURE_SUBSCRIPTION_ID="${AZURE_SUBSCRIPTION_ID:-$(az account show --query id -o tsv 2>/dev/null || echo '')}"

# Service name defaults (override to match your deployed services)
INIT_CR_SERVICE="${INIT_CR_SERVICE:-nina-cloudrun-init}"
INIT_CR_SIDECAR="${INIT_CR_SIDECAR:-nina-cloudrun-sidecar}"
INIT_CR_JOB="${INIT_CR_JOB:-nina-cloudrun-job}"
INIT_CR_FN_SIDECAR="${INIT_CR_FN_SIDECAR:-nina-cloudrun-function-sidecar}"
INIT_ACA_INIT="${INIT_ACA_INIT:-nina-containerapp-init}"
INIT_ACA_SIDECAR="${INIT_ACA_SIDECAR:-nina-containerapp-sidecar}"
INIT_AAS_CONTAINER="${INIT_AAS_CONTAINER:-nina-webapp-container}"
INIT_AAS_SIDECAR="${INIT_AAS_SIDECAR:-nina-webapp-sidecar}"
INIT_AAS_CODE="${INIT_AAS_CODE:-nina-webapp-linux-code}"
COMPAT_AZURE_APP="${COMPAT_AZURE_APP:-nina-compat-inventory-node}"
COMPAT_GCP_FN="${COMPAT_GCP_FN:-nina-compat-inventory-nodejs}"
COMPAT_GCP_PROJECT="${COMPAT_GCP_PROJECT:-datadog-sandbox}"
AZURE_RG_ACA="${AZURE_RG_ACA:-dd-serverless-test-aca}"
AZURE_RG_AAS="${AZURE_RG_AAS:-dd-serverless-test-aas}"
COMPAT_AZURE_RG="${COMPAT_AZURE_RG:-self-monitoring-nina-dev}"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]:-$0}")" && pwd)"
OUTPUT_FILE="${OUTPUT_FILE:-${SCRIPT_DIR}/diagnostic-results-$(date -u +%Y%m%d-%H%M%S).txt}"

log() { echo "[$(date -u '+%Y-%m-%dT%H:%M:%SZ')] $*"; }

# ---------------------------------------------------------------------------
# GCP log fetcher — matches inventory log lines containing resource_id
# ---------------------------------------------------------------------------
_gcp_logs() {
  local label="$1" resource_type="$2" filter_key="$3" filter_val="$4" project="${5:-${GCP_PROJECT}}"
  echo ""
  echo "================================================================"
  echo "# ${label}"
  echo "================================================================"
  gcloud logging read \
    "resource.type=${resource_type} AND resource.labels.${filter_key}=${filter_val} AND (textPayload:resource_id OR textPayload:SERVERLESS_DIAGNOSTIC OR textPayload:\"Inventory payload\")" \
    --project="${project}" \
    --limit=50 \
    --format="value(textPayload)" 2>/dev/null \
  || echo "  (no matching logs — trigger a request and retry)"
}

# ---------------------------------------------------------------------------
# Azure Container App log fetcher
# ---------------------------------------------------------------------------
_azure_containerapp_logs() {
  local label="$1" app_name="$2" rg="$3" container="${4:-}"
  [[ -z "${AZURE_SUBSCRIPTION_ID}" ]] && { echo "  (AZURE_SUBSCRIPTION_ID not set — skipping)"; return; }
  echo ""
  echo "================================================================"
  echo "# ${label}"
  echo "================================================================"
  local args=(--name "${app_name}" --resource-group "${rg}" \
               --subscription "${AZURE_SUBSCRIPTION_ID}" --tail 200)
  [[ -n "${container}" ]] && args+=(--container "${container}")
  az containerapp logs show "${args[@]}" 2>/dev/null \
  | grep -E "resource_id|SERVERLESS_DIAGNOSTIC|Inventory payload" \
  || echo "  (no matching logs — trigger a request and retry)"
}

# ---------------------------------------------------------------------------
# Azure Web App log fetcher (downloads archived logs)
# ---------------------------------------------------------------------------
_azure_webapp_logs() {
  local label="$1" app_name="$2" rg="$3"
  [[ -z "${AZURE_SUBSCRIPTION_ID}" ]] && { echo "  (AZURE_SUBSCRIPTION_ID not set — skipping)"; return; }
  echo ""
  echo "================================================================"
  echo "# ${label}"
  echo "================================================================"
  local tmp_zip
  tmp_zip=$(mktemp /tmp/webapp-logs-XXXXXX.zip)
  az webapp log download \
    --name "${app_name}" \
    --resource-group "${rg}" \
    --subscription "${AZURE_SUBSCRIPTION_ID}" \
    --log-file "${tmp_zip}" 2>/dev/null \
  && unzip -p "${tmp_zip}" 2>/dev/null \
     | grep -aE "resource_id|SERVERLESS_DIAGNOSTIC|Inventory payload" \
     | sort -u \
  || echo "  (no matching logs — trigger a request and retry)"
  rm -f "${tmp_zip}"
}

# ---------------------------------------------------------------------------
# Azure Function log fetcher (compat)
# ---------------------------------------------------------------------------
_azure_function_logs() {
  local label="$1" app_name="$2" rg="$3"
  [[ -z "${AZURE_SUBSCRIPTION_ID}" ]] && { echo "  (AZURE_SUBSCRIPTION_ID not set — skipping)"; return; }
  echo ""
  echo "================================================================"
  echo "# ${label}"
  echo "================================================================"
  # az webapp log tail exits after --timeout seconds; we want a snapshot not a stream.
  # Use log download instead for reliability.
  local tmp_zip
  tmp_zip=$(mktemp /tmp/fn-logs-XXXXXX.zip)
  az webapp log download \
    --name "${app_name}" \
    --resource-group "${rg}" \
    --subscription "${AZURE_SUBSCRIPTION_ID}" \
    --log-file "${tmp_zip}" 2>/dev/null \
  && unzip -p "${tmp_zip}" 2>/dev/null \
     | grep -aE "resource_id|workload_type|Inventory payload sent" \
     | sort -u \
  || echo "  (no matching logs — trigger a request and retry)"
  rm -f "${tmp_zip}"
}

# ---------------------------------------------------------------------------
# Run all checks
# ---------------------------------------------------------------------------
run_checks() {
  echo "================================================================"
  echo "SVLS-9604 — Serverless REDAPL Diagnostic Log Collection"
  echo "Date: $(date -u '+%Y-%m-%d %H:%M:%S UTC')"
  echo "================================================================"
  echo ""
  echo "What to look for: lines containing 'resource_id=//...' — this is"
  echo "the CCRID used as the REDAPL table key, NOT the agent UUID."
  echo ""

  echo "===== serverless_init_agent workloads ====="

  _gcp_logs "SI-01 Cloud Run Service (in-container, ${INIT_CR_SERVICE})" \
    cloud_run_revision service_name "${INIT_CR_SERVICE}"

  _gcp_logs "SI-02 Cloud Run Service (sidecar, ${INIT_CR_SIDECAR})" \
    cloud_run_revision service_name "${INIT_CR_SIDECAR}"

  echo ""
  echo "# SI-03 Cloud Run Job — requires separate POC (not deployed via self-monitoring)"
  _gcp_logs "SI-03 Cloud Run Job (${INIT_CR_JOB}) — if POC deployed" \
    cloud_run_job job_name "${INIT_CR_JOB}"

  echo ""
  echo "# SI-04 Cloud Run Functions Gen 2 — requires separate POC"
  _gcp_logs "SI-04 Cloud Run Functions Gen 2 (${INIT_CR_FN_SIDECAR}) — if POC deployed" \
    cloud_run_revision service_name "${INIT_CR_FN_SIDECAR}"

  _azure_containerapp_logs "SI-05 Azure Container App (in-container, ${INIT_ACA_INIT})" \
    "${INIT_ACA_INIT}" "${AZURE_RG_ACA}"

  _azure_containerapp_logs "SI-06 Azure Container App (sidecar, dd-agent, ${INIT_ACA_SIDECAR})" \
    "${INIT_ACA_SIDECAR}" "${AZURE_RG_ACA}" "dd-agent"

  _azure_webapp_logs "SI-07 Azure App Service Linux container (${INIT_AAS_CONTAINER})" \
    "${INIT_AAS_CONTAINER}" "${AZURE_RG_AAS}"

  _azure_webapp_logs "SI-08 Azure App Service SITECONTAINERS (${INIT_AAS_SIDECAR})" \
    "${INIT_AAS_SIDECAR}" "${AZURE_RG_AAS}"

  _azure_webapp_logs "SI-09 Azure App Service Linux code (${INIT_AAS_CODE})" \
    "${INIT_AAS_CODE}" "${AZURE_RG_AAS}"

  echo ""
  echo "===== serverless_compat_agent workloads ====="

  _azure_function_logs "SC-01 Azure Functions Node.js (${COMPAT_AZURE_APP})" \
    "${COMPAT_AZURE_APP}" "${COMPAT_AZURE_RG}"

  _gcp_logs "SC-02 GCP Cloud Functions Gen 1 (${COMPAT_GCP_FN})" \
    cloud_function function_name "${COMPAT_GCP_FN}" "${COMPAT_GCP_PROJECT}"
}

# ---------------------------------------------------------------------------
# Extract resource_ids and generate DDSQL queries
# ---------------------------------------------------------------------------
generate_sql() {
  local output_file="$1"

  # Extract resource_ids — CCRID format: //run.googleapis.com/... or //cloudfunctions.googleapis.com/... etc.
  local resource_ids
  resource_ids=$(python3 -c "
import re, sys
text = open('${output_file}').read()
# resource_id= or resource_id: followed by a CCRID
rids = re.findall(r'resource_id[=:]\s*(//[^\s,\"\']+)', text)
print('\n'.join(sorted(set(rids))))
" 2>/dev/null || true)

  echo ""
  echo "================================================================"
  echo "# DDSQL Queries (paste in go/redapl → Queries → SQL)"
  echo "# Note: these tables do NOT have api_key_uuid — filter by resource_id."
  echo "================================================================"
  echo ""

  if [[ -z "${resource_ids}" ]]; then
    echo "-- No resource_ids found in logs yet."
    echo "-- Trigger the services, wait ~5 min for EPRW propagation, and retry."
    echo ""
    echo "-- Fallback: show all rows modified today"
    echo "SELECT resource_id, workload_type, deployment_model, _modified_at"
    echo "FROM udm.all.serverless_init_agent"
    echo "WHERE _modified_at >= TIMESTAMP '$(date -u +%Y-%m-%dT00:00:00Z)'"
    echo "ORDER BY _modified_at DESC LIMIT 20;"
    echo ""
    echo "SELECT resource_id, workload_type, _modified_at"
    echo "FROM udm.all.serverless_compat_agent"
    echo "WHERE _modified_at >= TIMESTAMP '$(date -u +%Y-%m-%dT00:00:00Z)'"
    echo "ORDER BY _modified_at DESC LIMIT 20;"
    return
  fi

  # Build IN clause
  local in_clause=""
  while IFS= read -r rid; do
    [[ -z "${rid}" ]] && continue
    in_clause+="    '${rid}',\n"
  done <<< "${resource_ids}"

  echo "-- serverless_init_agent: rows by extracted resource_id"
  echo "SELECT _key, resource_id, resource_name, workload_type, deployment_model,"
  echo "       agent_version_base, serverless_init_version, runtime,"
  echo "       _first_seen_at, _modified_at"
  echo "FROM udm.all.serverless_init_agent"
  echo "WHERE resource_id IN ("
  printf "%b" "${in_clause}"
  echo ")"
  echo "ORDER BY workload_type, _modified_at DESC;"
  echo ""

  echo "-- serverless_compat_agent: rows by extracted resource_id"
  echo "SELECT _key, resource_id, resource_name, workload_type,"
  echo "       serverless_compat_version, serverless_compat_runtime_version,"
  echo "       _first_seen_at, _modified_at"
  echo "FROM udm.all.serverless_compat_agent"
  echo "WHERE resource_id IN ("
  printf "%b" "${in_clause}"
  echo ")"
  echo "ORDER BY workload_type, _modified_at DESC;"
  echo ""

  echo "-- Cardinality: rows must equal distinct_resources"
  echo "SELECT 'serverless_init_agent' AS flavor, workload_type,"
  echo "       COUNT(*) AS total_rows, COUNT(DISTINCT resource_id) AS distinct_resources"
  echo "FROM udm.all.serverless_init_agent"
  echo "WHERE resource_id IN ("
  printf "%b" "${in_clause}"
  echo ")"
  echo "GROUP BY workload_type"
  echo "UNION ALL"
  echo "SELECT 'serverless_compat_agent', workload_type,"
  echo "       COUNT(*), COUNT(DISTINCT resource_id)"
  echo "FROM udm.all.serverless_compat_agent"
  echo "WHERE resource_id IN ("
  printf "%b" "${in_clause}"
  echo ")"
  echo "GROUP BY workload_type"
  echo "ORDER BY flavor, workload_type;"
}

# ---------------------------------------------------------------------------
# Entry point
# ---------------------------------------------------------------------------
log "Collecting diagnostic logs → ${OUTPUT_FILE}"
run_checks | tee "${OUTPUT_FILE}"
generate_sql "${OUTPUT_FILE}" | tee -a "${OUTPUT_FILE}"

echo ""
log "Done. Output: ${OUTPUT_FILE}"

# Summarise what was found
found=$(python3 -c "
import re
text = open('${OUTPUT_FILE}').read()
rids = sorted(set(re.findall(r'resource_id[=:]\s*(//[^\s,\"\']+)', text)))
if rids:
    print(f'{len(rids)} resource_id(s) found:')
    print('\n'.join('  ' + r for r in rids))
else:
    print('No resource_ids found. Trigger the services and retry.')
" 2>/dev/null || echo "Could not parse output file.")

log "${found}"

if [[ "${found}" == "No resource_ids found."* ]]; then
  exit 1
fi
