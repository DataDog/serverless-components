# Datadog FIPS for Serverless

A package to support FIPS builds for serverless tools. Currently tested with
the datadog-lambda-extension, but it may be useful in other environments.

Please add the following to your `clippy.toml`:

```
disallowed-methods = [
  { path = "reqwest::Client::builder", reason = "prefer the FIPS-compatible adapter", replacement = "datadog_fips::reqwest_adapter::create_reqwest_client_builder" },
]
```
