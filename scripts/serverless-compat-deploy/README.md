# serverless-compat deploy scripts

End-to-end scripts for building, deploying, and verifying the `datadog-serverless-compat` binary across the two test functions used for REDAPL/Fleet Automation validation (SVLS-9604).

## Test functions

| # | Platform | Function | URL |
|---|---|---|---|
| 1 | Azure Functions (Linux Consumption, Node.js) | `nina-compat-inventory-node` | `https://nina-compat-inventory-node.azurewebsites.net/api/httptest` |
| 2 | GCP Cloud Functions gen1 (Node.js) | `nina-compat-inventory-nodejs` | `https://us-central1-datadog-sandbox.cloudfunctions.net/nina-compat-inventory-nodejs` |

## Workflow

```
# 1. Build binary + pack npm package
./build.sh

# 2. Deploy to both test functions
DD_API_KEY=<key> ./deploy.sh

# 3. Trigger both functions to fire inventory payloads
./trigger.sh

# 4. Print DDSQL queries (run after ~1hr for rows to materialise)
./check-redapl.sh
```

## Prerequisites

- `cargo` with the `x86_64-unknown-linux-musl` target installed:
  ```
  rustup target add x86_64-unknown-linux-musl
  ```
- `az` CLI logged in (`az login`)
- `gcloud` CLI logged in (`gcloud auth login`)
- `serverless-compat-self-monitoring` repo cloned to `~/dd/serverless-compat-self-monitoring`
  (override with `SELF_MONITORING_DIR=/path/to/repo`)

## Environment variables

| Variable | Default | Description |
|---|---|---|
| `DD_API_KEY` | **required** | Datadog API key for the test functions |
| `DD_SITE` | `datadoghq.com` | Datadog site |
| `GCP_PROJECT` | `datadog-sandbox` | GCP project |
| `GCP_REGION` | `us-central1` | GCP region |
| `AZURE_SUBSCRIPTION_ID` | auto from `az account show` | Azure subscription |
| `AZURE_RESOURCE_GROUP` | `self-monitoring-nina-dev` | Azure resource group |
| `AZURE_FUNCTION_APP` | `nina-compat-inventory-node` | Azure function app name |
| `GCP_FUNCTION_NAME` | `nina-compat-inventory-nodejs` | GCP function name |
| `SELF_MONITORING_DIR` | `~/dd/serverless-compat-self-monitoring` | Checkout of the self-monitoring test repo |

## What lands in DDSQL

After a successful deploy + trigger, rows appear in `UDM.all.datadog_agent` with:

| Column | Value |
|---|---|
| `install_method_tool` | `serverless-compat` |
| `agent_version` | `7.83.0` (hardcoded; real compat version is in `serverless_compat_version`) |
| `flavor` | `agent` |
| `hostname` | `null` (EPRW nullifies it for serverless workloads) |

Fields like `workload_type`, `runtime_name`, `serverless_compat_version` are sent in
`agent_metadata` but don't map to any `datadog_agent` column — they're preserved in the
raw payload but not queryable until a dedicated `serverless_compat_agent` table exists
(see design doc SVLS-9604).
