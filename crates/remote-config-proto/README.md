# Remote Config Proto

This crate provides the generated Rust types for the Datadog Remote Configuration
protocol buffers. The definitions originate from
`pkg/proto/datadog/remoteconfig/remoteconfig.proto` in the Datadog Agent
repository and are compiled at build time using `prost-build`.

All message types derive `serde::Serialize` and `serde::Deserialize` so they can
be easily encoded to or decoded from JSON when bridging HTTP requests in higher
level services.
