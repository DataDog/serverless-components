# Datadog FIPS for Serverless

Crate which provides utils to build FIPS compliant components.

Please add the following to your `clippy.toml`:

```
disallowed-methods = [
  { path = "reqwest::Client::builder", reason = "prefer the FIPS-compatible adapter", replacement = "datadog_fips::reqwest_adapter::create_reqwest_client_builder" },
]
```
