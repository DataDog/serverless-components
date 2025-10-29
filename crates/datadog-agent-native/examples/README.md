# FFI Examples

This directory contains examples demonstrating how to use the Datadog Agent Native library from different languages.

## C Example

The C example (`example.c`) demonstrates the basic FFI interface:

1. **Create configuration** with `datadog_config_new()`
2. **Configure settings** (API key, site, features)
3. **Create agent** with `datadog_agent_new()`
4. **Start services** with `datadog_agent_start()`
5. **Wait for shutdown** with `datadog_agent_wait_for_shutdown()`
6. **Cleanup** with `datadog_agent_shutdown()` and `datadog_agent_free()`

### Building and Running

```bash
# Build the Rust library
cd ..
cargo build

# Compile the C example
cd examples
gcc -o example example.c -L../target/debug -ldatadog_agent_native -lpthread -ldl -lm

# Run the example
LD_LIBRARY_PATH=../target/debug ./example
```

### Environment Variables

- `DD_API_KEY`: Your Datadog API key (optional for testing)

## Other Language Examples

The same FFI interface can be used from other languages:

### Python (ctypes)

```python
import ctypes
import os

# Load the library
lib = ctypes.CDLL('./target/debug/libdatadog_agent_native.so')

# Define function signatures
lib.datadog_config_new.restype = ctypes.c_void_p
lib.datadog_agent_new.argtypes = [ctypes.c_void_p]
lib.datadog_agent_new.restype = ctypes.c_void_p

# Create and configure agent
config = lib.datadog_config_new()
lib.datadog_config_set_api_key(config, b"your-api-key")
agent = lib.datadog_agent_new(config)
lib.datadog_agent_start(agent)
lib.datadog_agent_wait_for_shutdown(agent)
lib.datadog_agent_shutdown(agent)
lib.datadog_agent_free(agent)
```

### Go (cgo)

```go
package main

/*
#cgo LDFLAGS: -L./target/debug -ldatadog_agent_native
#include "datadog_agent_native.h"
*/
import "C"
import "fmt"

func main() {
    config := C.datadog_config_new()
    defer C.datadog_config_free(config)

    C.datadog_config_set_api_key(config, C.CString("your-api-key"))

    agent := C.datadog_agent_new(config)
    defer C.datadog_agent_free(agent)

    if err := C.datadog_agent_start(agent); err != C.Ok {
        fmt.Println("Failed to start agent")
        return
    }

    C.datadog_agent_wait_for_shutdown(agent)
    C.datadog_agent_shutdown(agent)
}
```

### Node.js (ffi-napi)

```javascript
const ffi = require('ffi-napi');
const ref = require('ref-napi');

const lib = ffi.Library('./target/debug/libdatadog_agent_native', {
  'datadog_config_new': ['pointer', []],
  'datadog_config_set_api_key': ['int', ['pointer', 'string']],
  'datadog_agent_new': ['pointer', ['pointer']],
  'datadog_agent_start': ['int', ['pointer']],
  'datadog_agent_wait_for_shutdown': ['int', ['pointer']],
  'datadog_agent_shutdown': ['int', ['pointer']],
  'datadog_agent_free': ['void', ['pointer']]
});

const config = lib.datadog_config_new();
lib.datadog_config_set_api_key(config, 'your-api-key');

const agent = lib.datadog_agent_new(config);
lib.datadog_agent_start(agent);
lib.datadog_agent_wait_for_shutdown(agent);
lib.datadog_agent_shutdown(agent);
lib.datadog_agent_free(agent);
```

## Memory Management

⚠️ **Important**: Proper memory management is critical when using the FFI:

1. **Config**: Created with `datadog_config_new()`, consumed by `datadog_agent_new()`
   - Do NOT call `datadog_config_free()` after passing to `datadog_agent_new()`

2. **Agent**: Created with `datadog_agent_new()`, must be freed with `datadog_agent_free()`
   - Always call `datadog_agent_shutdown()` before `datadog_agent_free()`

3. **Strings**:
   - Strings passed TO the library are copied (caller can free)
   - Strings returned FROM the library are owned by Rust (do NOT free)

## Error Handling

All FFI functions return a `DatadogError` enum:

- `Ok = 0`: Success
- `NullPointer = 1`: Null pointer provided
- `InvalidString = 2`: Invalid UTF-8 string
- `ConfigError = 3`: Configuration error
- `InitError = 4`: Initialization error
- `StartupError = 5`: Startup error
- `ShutdownError = 6`: Shutdown error
- `RuntimeError = 7`: Runtime error
- `UnknownError = 99`: Unknown error

Always check return values:

```c
DatadogError error = datadog_agent_start(agent);
if (error != Ok) {
    fprintf(stderr, "Error: %d\n", error);
    // Handle error
}
```
