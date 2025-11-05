# Logging Setup Summary

## What Was Implemented

✅ **Flexible logging configuration** via `LogConfig` struct
✅ **File logging** with automatic daily rotation
✅ **JSON format** support for log aggregation systems
✅ **Dual output** - files and stdout simultaneously
✅ **Non-blocking writes** for performance
✅ **Environment variable control** for all options
✅ **Module-level filtering** (routini vs pingora)

## Files Modified/Created

1. **src/utils/tracing.rs** - Enhanced logging implementation
2. **src/main.rs** - Updated to use new logging config
3. **Cargo.toml** - Added `tracing-appender` dependency
4. **LOGGING.md** - Complete logging documentation
5. **docs/LOGGING_QUICK_REFERENCE.md** - Quick reference card
6. **examples/logging_examples.rs** - Code examples
7. **examples/logging_demo.sh** - Interactive demo script

## Key Features

### 1. Environment Variables

| Variable | Purpose | Example |
|----------|---------|---------|
| `RUST_LOG` | Filter log levels | `info,routini=debug,pingora=info` |
| `LOG_DIR` | Enable file logging | `/var/log/routini` |
| `LOG_JSON` | JSON format | `1` or `true` |
| `NO_COLOR` | Disable ANSI colors | `1` |

### 2. File Rotation

When `LOG_DIR` is set:
- New file created daily at midnight UTC
- Format: `routini.log`, `routini.log.2025-01-15`, etc.
- Non-blocking writes to avoid performance impact
- Automatic cleanup can be handled by logrotate

### 3. Output Modes

**Development (stdout only):**
```bash
cargo run
```

**Production (file + stdout):**
```bash
LOG_DIR=/var/log/routini cargo run
```

**Production (JSON to file, plain to stdout):**
```bash
LOG_DIR=/var/log/routini LOG_JSON=1 cargo run
```

### 4. Module Filtering

Control log levels per module:
```bash
# Everything at INFO, routini at DEBUG
RUST_LOG=info,routini=debug

# Specific module at TRACE
RUST_LOG=info,routini::proxy=trace

# Multiple modules
RUST_LOG=routini::proxy=debug,routini::load_balancing=trace,pingora::lb=info
```

## Usage Examples

### Local Development
```bash
# Default - INFO level, stdout only
cargo run

# Debug your code, keep Pingora quiet
RUST_LOG=routini=debug,pingora=info cargo run
```

### Production Deployment
```bash
# Create log directory
mkdir -p /var/log/routini
chmod 755 /var/log/routini

# Run with file logging
RUST_LOG=info,routini=debug \
LOG_DIR=/var/log/routini \
LOG_JSON=1 \
NO_COLOR=1 \
./routini
```

### Docker
```dockerfile
ENV RUST_LOG=info,routini=debug
ENV LOG_DIR=/var/log/routini
ENV LOG_JSON=1
ENV NO_COLOR=1
```

### Kubernetes
```yaml
env:
  - name: RUST_LOG
    value: "info,routini=debug,pingora=info"
  - name: LOG_DIR
    value: "/var/log/routini"
  - name: LOG_JSON
    value: "1"
  - name: NO_COLOR
    value: "1"
```

## Code Usage

```rust
use tracing::{info, debug, error, warn};

// Simple logging
info!("Server started");
debug!("Processing request");

// Structured logging (recommended)
info!(
    backend = %backend_addr,
    latency_ms = latency.as_millis(),
    status = 200,
    "Request completed"
);

// Error logging
error!(
    backend = %addr,
    error = %err,
    retry_count = 3,
    "Backend connection failed"
);

// Spans for grouping
let span = tracing::info_span!("request", id = %request_id);
let _guard = span.enter();
// All logs here will include the span context
```

## Integration with Log Aggregation

### Elasticsearch + Filebeat
```yaml
# filebeat.yml
filebeat.inputs:
  - type: log
    enabled: true
    paths:
      - /var/log/routini/*.log
    json.keys_under_root: true
```

### Loki + Promtail
```yaml
# promtail-config.yml
scrape_configs:
  - job_name: routini
    static_configs:
      - targets: [localhost]
        labels:
          job: routini
          __path__: /var/log/routini/*.log
    pipeline_stages:
      - json:
          expressions:
            level: level
            message: message
```

### Fluentd
```conf
<source>
  @type tail
  path /var/log/routini/routini.log
  pos_file /var/log/td-agent/routini.log.pos
  tag routini
  format json
</source>
```

## Performance Considerations

| Level | Overhead | When to Use |
|-------|----------|-------------|
| TRACE | ~15-20% | Temporary debugging only |
| DEBUG | ~5-10% | Development, targeted production debugging |
| INFO | ~1-2% | Production default |
| WARN/ERROR | <1% | Always safe |

**Best Practice:** Use module filtering to keep performance high
```bash
# Good - Only debug specific modules
RUST_LOG=info,routini::proxy=debug

# Bad - Everything at debug (slow)
RUST_LOG=debug
```

## Troubleshooting

### No logs appearing
```bash
RUST_LOG=debug cargo run
```

### Permission denied
```bash
sudo mkdir -p /var/log/routini
sudo chown $USER:$USER /var/log/routini
```

### Too verbose
```bash
# Reduce to WARN
RUST_LOG=warn cargo run

# Or filter specific modules
RUST_LOG=info,pingora=warn cargo run
```

### Logs not rotating
- Rotation happens at midnight UTC
- Check disk space: `df -h /var/log/routini`
- Check permissions: `ls -la /var/log/routini`

## Next Steps

1. **Test the setup:**
   ```bash
   # Run the logging examples
   cargo run --example logging_examples
   
   # Or run the interactive demo (requires full server setup)
   ./examples/logging_demo.sh
   ```

2. **Configure for your environment:**
   - Set `RUST_LOG` for appropriate verbosity
   - Set `LOG_DIR` for production file logging
   - Enable `LOG_JSON` if using log aggregation

3. **Set up log rotation** (optional):
   ```bash
   # /etc/logrotate.d/routini
   /var/log/routini/*.log {
       daily
       rotate 30
       compress
       delaycompress
       notifempty
       missingok
   }
   ```

4. **Monitor logs:**
   ```bash
   # Follow logs
   tail -f /var/log/routini/routini.log
   
   # Search logs
   grep "ERROR" /var/log/routini/*.log
   
   # Parse JSON logs
   cat /var/log/routini/routini.log | jq 'select(.level == "ERROR")'
   ```

## Further Reading

- Full documentation: `LOGGING.md`
- Quick reference: `docs/LOGGING_QUICK_REFERENCE.md`
- Code examples: `examples/logging_examples.rs`
- Demo script: `examples/logging_demo.sh`
