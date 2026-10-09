# Datadog Serverless Logging

Log filter setup shared by the serverless agents.

`build_env_filter` builds a `tracing-subscriber` filter from the agent's base directives and the
value of `DD_LOG_LEVEL_BY_TARGET`, a comma-separated list of `target=level` entries. For
example, `DD_LOG_LEVEL_BY_TARGET=dogstatsd=debug` turns on debug logs from the `dogstatsd`
crate only. Each agent keeps its own subscriber and log format.
