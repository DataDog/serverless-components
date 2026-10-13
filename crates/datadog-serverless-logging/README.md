# Datadog Serverless Logging

This crate lets a user set the log level of one part of a serverless agent, such as the Lambda
extension. Then the user can see the debug logs of that part without the debug logs of the
whole agent.

## Usage

Set the environment variable `DD_LOG_LEVEL_BY_TARGET` on the function, such as a Lambda
function. Its value is a comma-separated list of `<target>=<level>` entries. Each entry sets the
log level of one target, and the other targets keep the agent's default level. For example:

| Environment variable | Value |
| --- | --- |
| `DD_LOG_LEVEL_BY_TARGET` | `cold_start_duration=debug,dogstatsd=debug` |

With this value, the agent logs debug messages with the target `cold_start_duration`, and debug
messages from the `dogstatsd` crate.

The level is one of `off`, `error`, `warn`, `info`, `debug` and `trace`, in any case. The agent
logs a warning for an entry that is not valid, and ignores that entry. For example, `debug` with
no target is not valid.

## Targets

The agents log with [`tracing`](https://docs.rs/tracing), a Rust logging library. Every log
statement has a target. An entry matches each target that starts with the entry's text.

| Kind of target | Where the target comes from | Example entry |
| --- | --- | --- |
| Custom target | The statement sets it: `debug!(target: "cold_start_duration", ...)` | `cold_start_duration=debug` |
| Module of the agent | A statement with no `target:` gets its module path, such as `bottlecap::traces::trace_flusher` in the Lambda extension. The entry also matches the modules under it. | `bottlecap::traces=debug` |
| All of the agent's own code | The crate name matches all its module paths. The Lambda extension's crate is `bottlecap`. It does not match custom targets. | `bottlecap=debug` |
| Crate from this repository | Its module paths start with the crate name, with underscores in place of hyphens | `dogstatsd=debug`, `datadog_agent_config=debug` |
| Third-party crate | The same as for a crate from this repository | `hyper=debug` |

Because of the prefix match, do not give a custom target a name that starts with the name of
another target. For example, an entry for `trace_flush` also matches `trace_flush_duration`.

## Use in an agent

The agent calls the function `build_env_filter` once, when it sets up logging. The function
takes two inputs:

- The agent's base filter, in the same format. It turns off some targets and sets the default
  level. For example, `h2=off,info` turns off the `h2` logs and sets the default level to info.
- The value of `DD_LOG_LEVEL_BY_TARGET`.

It returns the filter, and the entries that are not valid. The agent gives the filter to
[`tracing-subscriber`](https://docs.rs/tracing-subscriber), the library that writes the agent's
logs. Then it logs a warning for the entries that are not valid. Each agent keeps its own log
format.

Only `tracing` events go through the filter. If a crate logs with the
[`log`](https://docs.rs/log) crate, the agent must install a bridge from `log` to `tracing`, for
example `tracing_log::LogTracer`.
