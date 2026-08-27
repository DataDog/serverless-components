#!/usr/bin/env bash
set -euo pipefail

# ---------------------------------------------------------------------------
# demo.sh — SVLS-9604 combined trigger: serverless-init + serverless-compat
#
# Triggers cold starts on all deployed self-monitoring services for both
# flavors so you can validate both REDAPL tables in one run.
#
# This script triggers only — it does NOT build or deploy.
# Run the self-monitoring repos' own build/deploy first:
#
#   serverless-init (all apps, every runtime):
#     cd ~/go/src/github.com/DataDog/serverless-init-self-monitoring
#     AGENT_IMAGE=gcr.io/datadoghq/serverless-init:<rc-tag> npm start
#
#   serverless-compat (Azure + GCP functions, RC environment):
#     cd ~/go/src/github.com/DataDog/serverless-compat-self-monitoring
#     poetry run build
#     ENVIRONMENT=rc DD_API_KEY=<key> poetry run deploy
#
# Usage:
#   export DD_API_KEY=<staging-key>
#   export GCP_PROJECT=datadog-serverless-gcp-demo
#   export AZURE_SUBSCRIPTION_ID=<sub-id>
#   ./demo.sh
#
# Skip flags:
#   SKIP_INIT_TRIGGER=true    — skip triggering serverless-init self-monitoring apps
#   SKIP_COMPAT_TRIGGER=true  — skip triggering serverless-compat functions
# ---------------------------------------------------------------------------

: "${DD_API_KEY:?DD_API_KEY must be set}"

DD_SITE="${DD_SITE:-datad0g.com}"
GCP_PROJECT="${GCP_PROJECT:-datadog-serverless-gcp-demo}"
GCP_REGION="${GCP_REGION:-us-central1}"
AZURE_FUNCTION_APP="${AZURE_FUNCTION_APP:-nina-compat-inventory-node}"
GCP_FUNCTION_NAME="${GCP_FUNCTION_NAME:-nina-compat-inventory-nodejs}"

SKIP_INIT_TRIGGER="${SKIP_INIT_TRIGGER:-false}"
SKIP_COMPAT_TRIGGER="${SKIP_COMPAT_TRIGGER:-false}"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]:-$0}")" && pwd)"

# Path to serverless-init-self-monitoring (provides npm run test).
INIT_SM_DIR="${INIT_SM_DIR:-${HOME}/go/src/github.com/DataDog/serverless-init-self-monitoring}"

log()    { echo "[$(date -u '+%Y-%m-%dT%H:%M:%SZ')] $*"; }
header() {
  echo ""
  echo "=================================================="
  echo "  $*"
  echo "=================================================="
}

TRIGGER_START=$(date -u '+%Y-%m-%dT%H:%M:%SZ')

# ---------------------------------------------------------------------------
# Stage 1 — Trigger serverless-init self-monitoring apps
#
# Uses `npm run test:once` from serverless-init-self-monitoring.
# Sends one request to each deployed app, reports results, and exits nonzero
# if any request fails. Does NOT cover all 9 RFC workloads — see coverage gap
# report printed by `npm run test:once` and `npm run burst`.
#
# Uncovered by this repo's self-monitoring apps (require separate POC):
#   SI-03 Cloud Run Jobs
#   SI-04 Cloud Run Functions Gen 2
# ---------------------------------------------------------------------------
if [[ "${SKIP_INIT_TRIGGER}" != "true" ]]; then
  header "Stage 1/2 — Trigger serverless-init self-monitoring apps (test:once)"

  if [[ ! -d "${INIT_SM_DIR}" ]]; then
    log "ERROR: serverless-init-self-monitoring not found at ${INIT_SM_DIR}"
    log "  Set INIT_SM_DIR or: gh repo clone DataDog/serverless-init-self-monitoring ${INIT_SM_DIR}"
    exit 1
  fi

  log "Running npm run test:once..."
  (cd "${INIT_SM_DIR}" && npm run test:once 2>&1)
  log "Init trigger complete."
else
  log "Skipping init trigger (SKIP_INIT_TRIGGER=true)"
fi

# ---------------------------------------------------------------------------
# Stage 2 — Trigger serverless-compat functions
#
# Uses trigger.sh which hits the Azure Function and GCP Cloud Function gen1
# deployed by serverless-compat-self-monitoring.
# ---------------------------------------------------------------------------
if [[ "${SKIP_COMPAT_TRIGGER}" != "true" ]]; then
  header "Stage 2/2 — Trigger serverless-compat functions"

  GCP_PROJECT="${GCP_PROJECT}" \
  GCP_REGION="${GCP_REGION}" \
  AZURE_FUNCTION_APP="${AZURE_FUNCTION_APP}" \
  GCP_FUNCTION_NAME="${GCP_FUNCTION_NAME}" \
    "${SCRIPT_DIR}/trigger.sh"
else
  log "Skipping compat trigger (SKIP_COMPAT_TRIGGER=true)"
fi

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------
header "Done — REDAPL validation"

echo "Triggered at : ${TRIGGER_START}"
echo "DD_SITE      : ${DD_SITE}"
echo ""
echo "EPRW metrics to watch in ${DD_SITE} Datadog org:"
echo "  sum:event_platform_resource_writer.agentmetadata.serverless_write.accepted{resource_type:serverless_init_agent}.as_count()"
echo "  sum:event_platform_resource_writer.agentmetadata.serverless_write.accepted{resource_type:serverless_compat_agent}.as_count()"
echo "  sum:event_platform_resource_writer.agentmetadata.serverless_rejected.missing_resource_id{*}.as_count()"
echo "  sum:event_platform_resource_writer.agentmetadata.serverless_rejected.missing_resource_name{*}.as_count()"
echo "  sum:event_platform_resource_writer.agentmetadata.serverless_rejected.missing_workload_type{*}.as_count()"
echo "  sum:event_platform_resource_writer.agentmetadata.serverless_rejected.invalid_workload_type{*}.as_count()"
echo ""
echo "DDSQL validation:"
echo "  ${SCRIPT_DIR}/check-redapl.sh"
echo "Log collection:"
echo "  ${SCRIPT_DIR}/check-logs.sh"
