# Datadog Serverless Logging

Log filter setup shared by the serverless agents.

`build_env_filter` builds a `tracing-subscriber` filter from the agent's base directives and the
value of `DD_LOG_LEVEL_BY_TARGET`. The value is a comma-separated list of `target=level`
entries. Each entry sets the log level of one target, and the other targets keep the agent's
default level. Each agent keeps its own subscriber and log format.

Set it as an environment variable of the function, such as a Lambda function. For example:

| Environment variable | Value |
| --- | --- |
| `DD_LOG_LEVEL_BY_TARGET` | `cold_start_duration=debug,dogstatsd=debug` |

With this value, the agent logs debug messages with the target `cold_start_duration`, and debug
messages from the `dogstatsd` crate.

## Targets

Every `tracing` statement has a target. An entry matches each target that starts with the
entry's text.

| Kind of target | Where the target comes from | Example entry |
| --- | --- | --- |
| Custom target | The statement sets it: `debug!(target: "cold_start_duration", ...)` | `cold_start_duration=debug` |
| Module of the agent | A statement with no `target:` gets its module path, such as `bottlecap::traces::trace_flusher`. The entry also matches the modules under it. | `bottlecap::traces=debug` |
| All of the agent's own code | The crate name matches all its module paths. It does not match custom targets. | `bottlecap=debug` |
| Crate from this repository | Its module paths start with the crate name, with underscores in place of hyphens | `dogstatsd=debug`, `datadog_agent_config=debug` |
| Third-party crate | The same as for a crate from this repository | `hyper=debug` |

Because of the prefix match, do not give a custom target a name that starts with the name of
another target. For example, an entry for `trace_flush` also matches `trace_flush_duration`.

Only `tracing` events are filtered. A crate that logs with the `log` crate needs a `log` bridge
in the agent.
