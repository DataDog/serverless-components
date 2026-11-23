# Parity report

- Binary: rust (/parity/out/bin/rust/datadog-init)
- Go references: datadog-agent commit 0123456789abcdef0123456789abcdef01234567, serverless-init release 1.7.8

## Parity bar

P0 scenarios passing: 2 of 4 (1 with known coverage gaps)

P1 differences:

- traces.sampling: stats[service=parity-app,name=web.request,resource=/].hits: Go 2, this binary 1

## Summary

| Scenario | Priority | Status | Known coverage gap |
|---|---|---|---|
| metrics.flush-cadence | P0 | fail |  |
| process.exit-codes | P0 | pass |  |
| traces.info-endpoint | P0 | pass | yes |
| performance.startup-and-memory | P2 | reported |  |
| process.signal-forwarding | P0 | harness-error |  |
| traces.sampling | P1 | fail |  |

## Scenarios

### metrics.flush-cadence

- Status: fail
- Priority: P0
- Features: metrics.flush-cadence

Differences (1):

```
timing.interval.series: Go "3s", this binary "10s"
```

Timing:

| Measure | Go | This binary |
|---|---|---|
| Flush interval of series sent by the workload | 3s | 10s |
| Longest delivery delay of series | 1.2s | 1.2s |
| Shutdown after SIGTERM | 400ms | 400ms |

Startup and memory:

| Measure | Go | This binary |
|---|---|---|
| Startup time | 900ms | 300ms |
| Peak memory | 60.0 MiB | 3.0 MiB |
| Killed with SIGKILL | no | yes |

### process.exit-codes

- Status: pass
- Priority: P0
- Features: process.exit-codes

Timing:

No timing measure was measurable.

Startup and memory:

| Measure | Go | This binary |
|---|---|---|
| Startup time | 800ms | not measurable |
| Peak memory | 50.0 MiB | unknown |

### traces.info-endpoint

- Status: pass
- Priority: P0
- Features: traces.info-endpoint
- Known coverage gap: The /info body is not compared.

Timing:

No timing measure was measurable.

Startup and memory:

| Measure | Go | This binary |
|---|---|---|
| Startup time | not measurable | not measurable |
| Peak memory | unknown | unknown |

### performance.startup-and-memory

- Status: reported
- Priority: P2
- Features: none
- Report only: measured and reported, never pass or fail

Differences (1):

```
sketches[metric=m,tags=].count: Go 1, this binary 2
```

Timing:

No timing measure was measurable.

Startup and memory:

| Measure | Go | This binary |
|---|---|---|
| Startup time | 100ms | 200ms |
| Peak memory | unknown | unknown |

### process.signal-forwarding

- Status: harness-error
- Priority: P0
- Features: process.signal-forwarding
- Error: starting parity-process-signal-forwarding-rust: docker compose up failed

### traces.sampling

- Status: fail
- Priority: P1
- Features: traces.sampling, traces.stats

Differences (2):

```
traces[priority=0,service=parity-app,name=web.request,resource=/]: Go absent, this binary {"spans":1} (expected: implementation.local-priority-0-drop)
stats[service=parity-app,name=web.request,resource=/].hits: Go 2, this binary 1
```

Timing:

No timing measure was measurable.

Startup and memory:

| Measure | Go | This binary |
|---|---|---|
| Startup time | not measurable | not measurable |
| Peak memory | unknown | unknown |
