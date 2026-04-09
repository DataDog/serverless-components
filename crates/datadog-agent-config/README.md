# datadog-agent-config

Shared configuration crate for Datadog serverless agents. Provides a typed `Config` struct with built-in loading from environment variables (`DD_*`) and YAML files (`datadog.yaml`), with environment variables taking precedence.

## Core features

- **Typed config struct** with fields for site, API key, proxy, logs, APM, metrics, DogStatsD, OTLP, and trace propagation
- **Two built-in sources**: `EnvConfigSource` (reads `DD_*` / `DATADOG_*` env vars) and `YamlConfigSource` (reads `datadog.yaml`)
- **Graceful deserialization**: every field uses forgiving deserializers that fall back to defaults on bad input, so one misconfigured value never crashes the whole config
- **Extensible via `ConfigExtension`**: consumers can define additional configuration fields without modifying this crate

## Quick start

```rust
use std::path::Path;
use datadog_agent_config::get_config;

let config = get_config(Path::new("/var/task"));
println!("site: {}", config.site);
println!("api_key: {}", config.api_key);
```

## Extensible configuration

Consumers that need additional fields (e.g., Lambda-specific settings) implement the `ConfigExtension` trait instead of forking or copy-pasting the crate.

### 1. Define the extension and its source

```rust
use datadog_agent_config::{
    ConfigExtension, merge_fields,
    deserialize_optional_string, deserialize_optional_bool_from_anything,
};
use serde::Deserialize;

#[derive(Debug, PartialEq, Clone)]
pub struct MyExtension {
    pub custom_flag: bool,
    pub custom_name: String,
}

impl Default for MyExtension {
    fn default() -> Self {
        Self { custom_flag: false, custom_name: String::new() }
    }
}

/// Source struct for deserialization.
///
/// REQUIRED: `#[serde(default)]` on the struct + graceful deserializers on each
/// field. Without these, a missing or malformed value fails the entire extension
/// extraction — fields silently fall back to defaults with a warning log.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct MySource {
    #[serde(deserialize_with = "deserialize_optional_bool_from_anything")]
    pub custom_flag: Option<bool>,
    #[serde(deserialize_with = "deserialize_optional_string")]
    pub custom_name: Option<String>,
}

impl ConfigExtension for MyExtension {
    type Source = MySource;

    fn merge_from(&mut self, source: &MySource) {
        merge_fields!(self, source,
            string: [custom_name],
            value:  [custom_flag],
        );
    }
}
```

### 2. Load config with the extension

```rust
use std::path::Path;
use datadog_agent_config::{Config, get_config_with_extension};

type MyConfig = Config<MyExtension>;

let config: MyConfig = get_config_with_extension(Path::new("/var/task"));

// Core fields
println!("site: {}", config.site);

// Extension fields
println!("custom_flag: {}", config.ext.custom_flag);
println!("custom_name: {}", config.ext.custom_name);
```

Extension fields are populated from both `DD_*` environment variables and `datadog.yaml` using dual extraction: the core fields and extension fields are extracted independently from the same figment instance, so they don't interfere with each other.

### Flat fields only

The single `Source` type is used for both env var and YAML extraction. This works because Figment uses a single key-value namespace per provider, so flat fields map naturally to both `DD_*` env vars and top-level YAML keys. If you need nested YAML structures (e.g., `lambda: { enhanced_metrics: true }`) that differ from the flat env var layout, you'd need separate source structs — implement `merge_from` with a nested source struct and handle the mapping manually.

### Field name collisions

Extension fields are extracted independently from the same figment as core fields. If an extension defines a field with the same name as a core field (e.g., `api_key`), both get their own copy — they don't interfere, but the extension copy does **not** override the core value. Avoid shadowing core field names to prevent confusion.

### merge_fields! macro

The `merge_fields!` macro reduces boilerplate in `merge_from` by batching fields by merge strategy:

- `string`: merges `Option<String>` into `String` (sets value if `Some`)
- `value`: merges `Option<T>` into `T` (sets value if `Some`)
- `option`: merges `Option<T>` into `Option<T>` (overwrites if `Some`)

Custom merge logic (e.g., OR-ing two boolean fields together) goes after the macro call in the same method.

## Config loading precedence

1. `Config::default()` (hardcoded defaults)
2. `datadog.yaml` values (lower priority)
3. `DD_*` environment variables (highest priority)
4. Post-processing defaults (site, proxy, logs/APM URL construction)
