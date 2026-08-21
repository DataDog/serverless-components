#!/usr/bin/env bash
set -euo pipefail

# ---------------------------------------------------------------------------
# demo.sh — SVLS-9604 combined serverless-init + serverless-compat demo
#
# Builds and deploys serverless-compat test functions, then triggers cold
# starts on both compat (Azure + GCP) and init (all 9 services via the
# datadog-agent demo.sh) so you can validate both REDAPL tables in one run.
#
# Stages:
#   1. Build serverless-compat binary (cross-compile to linux/amd64)
#   2. Deploy compat functions (Azure Function + GCP Cloud Function gen1)
#   3. Trigger compat functions (fires inventory → serverless_compat_agent)
#   4. Trigger init services   (fires inventory → serverless_init_agent)
#   5. Print DDSQL queries for both tables
#
# Usage:
#   export DD_API_KEY=<your-staging-api-key>
#   export GCP_PROJECT=datadog-serverless-gcp-demo
#   export AZURE_SUBSCRIPTION_ID=1dd25961-a5c7-45bf-a5ba-c1475d365cc7
#   ./demo.sh
#
# Skip flags (combine freely):
#   SKIP_COMPAT_BUILD=true   — use existing binary; skip cargo build
#   SKIP_COMPAT_DEPLOY=true  — skip Azure/GCP compat function deployment
#   SKIP_INIT_TRIGGER=true   — skip triggering serverless-init services
#   SKIP_COMPAT_TRIGGER=true — skip triggering compat functions
#
# Minimal "trigger only" run (assumes everything is already deployed):
#   SKIP_COMPAT_BUILD=true SKIP_COMPAT_DEPLOY=true ./demo.sh
# ---------------------------------------------------------------------------

: "${DD_API_KEY:?DD_API_KEY must be set}"

DD_SITE="${DD_SITE:-datad0g.com}"
GCP_PROJECT="${GCP_PROJECT:-datadog-serverless-gcp-demo}"
GCP_REGION="${GCP_REGION:-us-central1}"
AZURE_SUBSCRIPTION_ID="${AZURE_SUBSCRIPTION_ID:-$(az account show --query id -o tsv 2>/dev/null || echo '')}"

SKIP_COMPAT_BUILD="${SKIP_COMPAT_BUILD:-false}"
SKIP_COMPAT_DEPLOY="${SKIP_COMPAT_DEPLOY:-false}"
SKIP_COMPAT_TRIGGER="${SKIP_COMPAT_TRIGGER:-false}"
SKIP_INIT_TRIGGER="${SKIP_INIT_TRIGGER:-false}"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]:-$0}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"

# Path to the datadog-agent demo.sh (for serverless-init triggering).
AGENT_DEMO="${HOME}/go/src/github.com/DataDog/datadog-agent/scripts/serverless-deploy/demo.sh"

log() { echo "[$(date -u '+%Y-%m-%dT%H:%M:%SZ')] $*"; }
header() {
  echo ""
  echo "=================================================="
  echo "  $*"
  echo "=================================================="
}

# ---------------------------------------------------------------------------
# Stage 1 — Build serverless-compat binary
# ---------------------------------------------------------------------------
if [[ "${SKIP_COMPAT_BUILD}" != "true" ]]; then
  header "Stage 1/4 — Build serverless-compat binary"
  GCP_PROJECT="${GCP_PROJECT}" \
  AZURE_SUBSCRIPTION_ID="${AZURE_SUBSCRIPTION_ID}" \
  DD_SITE="${DD_SITE}" \
    "${SCRIPT_DIR}/build.sh"
else
  log "Skipping compat build (SKIP_COMPAT_BUILD=true)"
fi

# ---------------------------------------------------------------------------
# Stage 2 — Deploy compat functions
# ---------------------------------------------------------------------------
if [[ "${SKIP_COMPAT_DEPLOY}" != "true" ]]; then
  header "Stage 2/4 — Deploy serverless-compat functions"
  DD_API_KEY="${DD_API_KEY}" \
  DD_SITE="${DD_SITE}" \
  GCP_PROJECT="${GCP_PROJECT}" \
  GCP_REGION="${GCP_REGION}" \
    "${SCRIPT_DIR}/deploy.sh"
else
  log "Skipping compat deploy (SKIP_COMPAT_DEPLOY=true)"
fi

# ---------------------------------------------------------------------------
# Stage 3 — Trigger serverless-compat cold starts
# ---------------------------------------------------------------------------
TRIGGER_START=$(date -u '+%Y-%m-%dT%H:%M:%SZ')

if [[ "${SKIP_COMPAT_TRIGGER}" != "true" ]]; then
  header "Stage 3/4 — Trigger serverless-compat cold starts"
  GCP_PROJECT="${GCP_PROJECT}" \
  GCP_REGION="${GCP_REGION}" \
    "${SCRIPT_DIR}/trigger.sh"
else
  log "Skipping compat trigger (SKIP_COMPAT_TRIGGER=true)"
fi

# ---------------------------------------------------------------------------
# Stage 4 — Trigger serverless-init services
# ---------------------------------------------------------------------------
if [[ "${SKIP_INIT_TRIGGER}" != "true" ]]; then
  if [[ -f "${AGENT_DEMO}" ]]; then
    header "Stage 4/4 — Trigger serverless-init services"
    SKIP_BUILD=true \
    SKIP_DEPLOY=true \
    GCP_PROJECT="${GCP_PROJECT}" \
    AZURE_SUBSCRIPTION_ID="${AZURE_SUBSCRIPTION_ID}" \
    DD_API_KEY="${DD_API_KEY}" \
      bash "${AGENT_DEMO}"
  else
    log "WARNING: datadog-agent demo.sh not found at ${AGENT_DEMO}"
    log "  Clone datadog-agent and set the path, or set SKIP_INIT_TRIGGER=true"
  fi
else
  log "Skipping init trigger (SKIP_INIT_TRIGGER=true)"
fi

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------
header "Done — REDAPL validation"

echo "Triggered at: ${TRIGGER_START}"
echo "Allow ~5 min for inventory payloads to reach EPRW and ~1hr for REDAPL."
echo ""
echo "Load check: check EPRW metrics in staging (datad0g.com) Datadog org:"
echo "  event_platform_resource_writer.agentmetadata.serverless_write.accepted"
echo "    {resource_type:serverless_compat_agent}"
echo "    {resource_type:serverless_init_agent}"
echo "  event_platform_resource_writer.agentmetadata.serverless_rejected.invalid_workload_type"
echo ""
echo "DDSQL validation queries:"
echo "  ${SCRIPT_DIR}/check-redapl.sh"
echo ""
cat <<'QUERIES'
-- Quick combined check (run in DDSQL against redapl/staging):

SELECT 'serverless_compat_agent' AS table_name,
       workload_type, resource_name, first_seen_at
FROM udm.all.serverless_compat_agent
WHERE api_key_uuid = '<your-api-key-uuid>'
ORDER BY first_seen_at DESC LIMIT 5;

SELECT 'serverless_init_agent' AS table_name,
       workload_type, resource_name, first_seen_at
FROM udm.all.serverless_init_agent
WHERE api_key_uuid = '<your-api-key-uuid>'
ORDER BY first_seen_at DESC LIMIT 5;

QUERIES
