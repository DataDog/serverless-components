# Serverless Components

A collection of libraries and binaries used for instrumenting AWS Lambda Functions, Azure Functions, and Azure Spring Apps.

## Compat inventory runtime metadata

With `DD_SERVERLESS_COMPAT_METADATA_ENABLED=true`, Compat wrappers pass the language
engine to the child binary using `DD_SERVERLESS_COMPAT_RUNTIME` (for example,
`python3.13.7`, not the wrapper package version).
This best-effort hint takes precedence over platform hints for the inventory `runtime`
field. Missing, empty, or `unknown` hints retain the existing platform-based detection for
older wrappers. Platform-owned environment variables are not overwritten.

This does not change `DD_SERVERLESS_COMPAT_VERSION`, which identifies the wrapper
package. Release a binary with this reader before bundling the updated wrappers;
older binaries do not read the new hint. Customers do not need to set it. Setting
`DD_SERVERLESS_COMPAT_METADATA_ENABLED=false` disables inventory reporting.

## Development

Install the git pre-push hook (runs `cargo fmt --check` and `cargo clippy -D warnings` before each push):

```sh
git config core.hooksPath .githooks
```