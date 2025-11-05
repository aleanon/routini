# Routini Logging Quick Reference

## Environment Variables Cheat Sheet

```bash
# Development
RUST_LOG=debug cargo run                                    # All modules at DEBUG
RUST_LOG=routini=debug,pingora=info cargo run              # Fine-grained control

# Production - Files
LOG_DIR=/var/log/routini cargo run                          # File logging with rotation
LOG_DIR=/var/log/routini LOG_JSON=1 cargo run               # JSON format
RUST_LOG=info LOG_DIR=/var/log/routini cargo run           # Custom filter + files

# Production - Full config
RUST_LOG=info,routini=debug \
LOG_DIR=/var/log/routini \
LOG_JSON=1 \
NO_COLOR=1 \
cargo run
```

## Common Debugging Scenarios

| Problem | Command |
|---------|---------|
| Routing not working | `RUST_LOG=routini::proxy=debug cargo run` |
| Backend selection issues | `RUST_LOG=routini::load_balancing=debug cargo run` |
| Health check problems | `RUST_LOG=routini::load_balancing::health_check=debug cargo run` |
| Strategy changes | `RUST_LOG=routini::adaptive_loadbalancer=debug cargo run` |
| Everything verbose | `RUST_LOG=trace cargo run` (very noisy!) |
| Pingora internals | `RUST_LOG=pingora=debug cargo run` |

## In Code Usage

```rust
use tracing::{info, debug, error, warn, trace};

// Basic
info!("Server started");

// With fields (structured)
info!(
    backend = %backend_addr,
    latency_ms = 15,
    "Request completed"
);

// Error with context
error!(
    backend = %addr,
    error = %err,
    "Connection failed"
);

// Debug only when enabled
debug!(route = %path, "Routing request");

// Span for grouping
let span = tracing::info_span!("request", id = request_id);
let _guard = span.enter();
```

## File Locations (when LOG_DIR set)

```
{LOG_DIR}/
├── routini.log              # Current day
├── routini.log.2025-01-14   # Yesterday
├── routini.log.2025-01-13   # Day before
└── ...
```

## Log Levels

| Level | When to Use | Performance Impact |
|-------|-------------|-------------------|
| TRACE | Deep debugging, step-by-step | High (~20%) |
| DEBUG | Development, troubleshooting | Medium (~10%) |
| INFO | Production default | Low (~2%) |
| WARN | Potential issues | Minimal |
| ERROR | Failures only | Minimal |

## JSON Output Format

```json
{
  "timestamp": "2025-01-15T10:30:45.123Z",
  "level": "INFO",
  "target": "routini::proxy",
  "fields": {
    "backend": "127.0.0.1:8080",
    "latency_ms": 15
  },
  "message": "Request completed"
}
```

## Docker Usage

```dockerfile
ENV RUST_LOG=info,routini=debug
ENV LOG_DIR=/var/log/routini
ENV LOG_JSON=1
ENV NO_COLOR=1
```

## Systemd Service

```ini
[Service]
Environment="RUST_LOG=info,routini=debug"
Environment="LOG_DIR=/var/log/routini"
Environment="LOG_JSON=1"
```

## Kubernetes ConfigMap

```yaml
apiVersion: v1
kind: ConfigMap
metadata:
  name: routini-config
data:
  RUST_LOG: "info,routini=debug,pingora=info"
  LOG_DIR: "/var/log/routini"
  LOG_JSON: "1"
  NO_COLOR: "1"
```

## Tips

- Start with `info` level, add `debug` for specific modules only
- Use file logging in production (LOG_DIR)
- Use JSON format for log aggregation (LOG_JSON=1)
- Disable colors in containers (NO_COLOR=1)
- Module filters are comma-separated: `module1=level,module2=level`
- More specific filters override general ones: `debug,pingora=info`
