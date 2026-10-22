# SVLS-9604 independent workload runner

This runner builds the candidate `serverless-init` binary from the local
`datadog-agent` checkout and the Compat package from the local
`serverless-components` and `datadog-serverless-compat-js` checkouts. It does
not invoke either self-monitoring repository.

Authentication is always supplied internally through:

```sh
dd-auth --site=datad0g.com --org-uuid=2
```

Preview either exact resource matrix without creating resources:

```sh
./scripts/svls9604/run.sh --profile gcp --plan
./scripts/svls9604/run.sh --profile azure --plan
```

Execute a profile:

```sh
./scripts/svls9604/run.sh --profile gcp --yes
./scripts/svls9604/run.sh --profile azure --yes
```

Rerunning the Azure command with the same `--run-id` and `RESULTS_DIR` resumes
the partial manifest: resources already deployed with the same candidate Agent
image are reused, and only missing resources are deployed. This avoids
repeating the multi-hour portion of an interrupted Azure run.

These commands run the full automated suite by default:

- L0: one baseline request or execution per deployed resource
- L1: 10 concurrent requests per HTTP init resource
- L2: 50 concurrent requests per HTTP init resource
- L3: 100 concurrent requests per HTTP init resource
- L4: one concurrent request per distinct HTTP resource
- L5: 15 minutes of unchanged traffic at one-minute intervals
- L6: create a fresh representative revision with request concurrency set to 1,
  immediately send 100 concurrent requests, and record the new revision CCRID
- L7: keep revisions A and B active, configure a 10/90 traffic split, send 100
  requests through the service URL, and probe each revision directly

Add `--scaling-matrix` to create additional representative revisions for:

- L8: minimum instances/replicas `0`, `5`, and `100`, with concurrency `1`
- L9: maximum instances/replicas `100`, `1000`, and `4000`, with concurrency
  `1` and 100-request pressure per configured maximum

L8 verifies whether each provider-started process sends startup inventory. L9
verifies that scale-out reports keep one row per revision; 100 requests do not
prove that the service reached a configured maximum above 100. Use provider
instance-start metrics and producer startup logs for the actual instance count.
The scaling matrix is opt-in because minimum `100` can create material cloud
cost, and a provider may reject a requested maximum that exceeds the project,
subscription, region, or environment quota.

```sh
./scripts/svls9604/run.sh --profile gcp --scaling-matrix --yes
./scripts/svls9604/run.sh --profile azure --scaling-matrix --yes
```

Use `--suite baseline` for deployment debugging without the load stages.

L6 is cold-start *pressure*, not an inferred cold-start count. The authoritative
instance/start count must come from provider or Agent startup logs for revision
B. HTTP attempts and successful responses are reported separately.

The GCP profile creates 44 resources: 14 Cloud Run in-container services, 14
Cloud Run sidecars, one Cloud Run job, 14 real Gen2 Cloud Functions with a
sidecar attached to the backing Cloud Run service, and one Gen1 Compat
function.

The Azure profile creates 63 resources: 14 Container Apps in-container, 14
Container Apps sidecar, 14 App Service container in-container, 14 App Service
container sidecar, six App Service Linux-code sidecars, and one Node.js Compat
Function App.

Azure defaults target the existing test infrastructure:

- Azure Functions: resource group `dd-serverless-test-aca` (the App Service test
  resource group does not permit Linux Dynamic workers)

- Container Apps: `dd-serverless-test-aca/dd-serverless-env`
- ACR: `ddsvlstestaca.azurecr.io`
- App Service: `dd-serverless-test-aas` and its container, sidecar, and
  Linux-code plans

Every default is overridable through the corresponding CLI option or
environment variable. Each run writes a mode-`0700` evidence directory under
`/tmp/svls9604-RUN_ID` with a machine-readable deployment manifest. Generated
parameter files are mode `0600` because they contain temporary secure values.

The runner sets `DD_ENV=svls9604-RUN_ID` and records `started_at`, `updated_at`,
and `completed_at` in the manifest. This makes a run isolatable in EPRW metrics
without using high-cardinality `resource_id` metric tags.

Current automated scope is deployment inventory, endpoint/job baseline
execution, L1-L7 load/revision stages, revision-aware expected identities, and
report generation. An HTTP response alone is not proof that inventory traversed
EPRW or deduplicated in Iris.

The runner always rebuilds the Serverless Compat binary before packaging it.
This prevents a stale `target/` artifact from being deployed under a new run ID.

Each run produces `serverless-redapl-rc-results.md` and `report.json`. To add
table and pipeline evidence after the run, place these files in the evidence
directory and rerun `report.py`:

- `serverless_init_agent.csv`
- `serverless_compat_agent.csv`
- `pipeline-evidence.json`

```sh
python3 scripts/svls9604/report.py --manifest /path/to/run-manifest.json
```

For GCP, export Cloud Run logs containing `inventory report queued` as JSON,
then build the event-level process/instance ledger and stage summary:

```sh
python3 scripts/svls9604/collect_producer_evidence.py \
  --manifest /path/to/run-manifest.json \
  --gcp-logs /path/to/producer-events.json
```

This writes `producer-instance-ledger.csv`, `producer-stage-summary.csv`, and
`producer-evidence.json`. The ledger preserves timestamp, stage, service,
revision, provider instance ID, Agent process ID, report reason, and resource
ID. It does not infer downstream EPRW or Iris outcomes.

`pipeline-evidence.json` supports overall, per-stage, and per-revision counts:

```json
{
  "eprw_commit": "deployed-sha",
  "iris_commit": "deployed-sha",
  "eprw_debug_tracking": true,
  "iris_upsert_telemetry": true,
  "producer_attempts": 0,
  "producer_reasons": {"startup": 0, "periodic": 0, "refresh": 0},
  "decoder_accepts": 0,
  "decoder_reasons": {"startup": 0, "periodic": 0, "refresh": 0},
  "resource_edge_successes": 0,
  "resource_edge_failures": 0,
  "iris_primary": {"CREATED": 0, "UPDATED": 0, "EXTENDED": 0, "IGNORED": 0, "ERROR": 0},
  "stages": {
    "L6": {"producer_attempts": 0, "decoder_accepts": 0, "resource_edge_successes": 0, "resource_edge_failures": 0}
  },
  "resources": {
    "REVISION_CCRID": {
      "producer_attempts": 0,
      "decoder_accepts": 0,
      "resource_edge_successes": 0,
      "iris_primary": {"CREATED": 0, "UPDATED": 0, "EXTENDED": 0, "IGNORED": 0, "ERROR": 0}
    }
  }
}
```

Replace every example zero with the observed value. Omit measurements that
were not collected; the report treats missing values as `NOT MEASURED` and
never turns them into a pass.

## Pipeline evidence for an RFC run

Before the run, temporarily enable full EPRW debug tracking for org 2 and the
two resource types:

```ini
[dd.event_platform_resource_writer.debug_tracking.track_type_resource_type:agentmetadata:serverless_init_agent:2]
sampling_rate = 1

[dd.event_platform_resource_writer.debug_tracking.track_type_resource_type:agentmetadata:serverless_compat_agent:2]
sampling_rate = 1
```

Also enable the Iris experiment `serverless-inventory-upsert-telemetry`. It
emits one structured log per serverless upsert with `resource_id`, hashed
resource identity, result, origin Kafka partition/offset, and `shadow_mode`.
Disable these temporary controls after the validation window.

Collect these counts for the manifest's exact time window:

1. Producer reports: Cloud logs containing `inventory report queued`, grouped
   by `reason` (`startup`, `periodic`, or `refresh`). Compat currently emits
   only `Inventory payload sent (report_reason=startup`. Group by cloud
   resource and revision.
2. Decoder accepts, grouped by `report_reason`:
   `event_platform_resource_writer.agentmetadata.serverless_write.accepted`
   filtered by the manifest's unique `dd_env`.
3. Resource-edge responses:
   `event_platform_resource_writer.agentmetadata.serverless_write.resource_edge_response`
   filtered by `dd_env`, grouped by `outcome`.
4. Iris outcomes: feature-gated `serverless inventory upsert result` logs,
   restricted to `shadow_mode:false` and the manifest resource names, grouped
   by `resource_id` and `result`.
5. Final rows: DDSQL rows from `udm.all.serverless_init_agent` and
   `udm.all.serverless_compat_agent` filtered by the manifest's exact `dd_env`.
   For Cloud Run and Azure Container Apps, compare revision `resource_id` and
   stable `parent_resource_id` against every identity in the manifest.

The hard gates are:

```text
decoder accepts = resource-edge ok + resource-edge failures
resource-edge ok = primary Iris CREATED + UPDATED + EXTENDED + IGNORED + ERROR
unique DDSQL resource_id count = manifest expected_resource_ids count
duplicate DDSQL keys = 0
```

Interpret Iris results per revision-scoped `resource_id`: `CREATED` is the first row,
`UPDATED` is a real configuration change, `EXTENDED` is an unchanged report
deduplicated to the existing row, and `IGNORED` is stale/out-of-order. A cold
start or new process UUID may increase report attempts, but must not increase
row cardinality within one revision. Creating revision B must create a second
row related to the same stable parent; instances of revision B must deduplicate
into that row. Use `--skip-burst` only for deployment debugging.
