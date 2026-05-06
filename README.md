# Serverless Components

A collection of libraries and binaries used for instrumenting AWS Lambda Functions, Azure Functions, and Azure Spring Apps.

## Development

Install the git pre-push hook (runs `cargo fmt --check` and `cargo clippy -D warnings` before each push):

```sh
git config core.hooksPath .githooks
```