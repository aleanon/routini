# Routini Logging Guide

Routini uses the `tracing` ecosystem for structured logging, which provides both human-readable and machine-parsable log output.

## Examples

Run the logging examples to see different logging styles in action:
```bash
cargo run --example logging_examples
```

## Quick Start

### Development (stdout only)
```bash
# Default: INFO level
cargo run

# Debug level for all modules
RUST_LOG=debug cargo run

# Fine-grained control
RUST_LOG=routini=debug,pingora=info cargo run
```

### Production (file logging with rotation)
```bash
# Log to /var/log/routini directory
LOG_DIR=/var/log/routini cargo run

# JSON format for log aggregation
LOG_DIR=/var/log/routini LOG_JSON=1 cargo run

# Custom log levels in production
RUST_LOG=info,routini=debug LOG_DIR=/var/log/routini cargo run
```

## Environment Variables

| Variable | Description | Default | Example |
|----------|-------------|---------|---------|
| `RUST_LOG` | Log level filter | `info,routini=debug,pingora=info` | `debug` or `routini::proxy=trace` |
| `LOG_DIR` | Directory for log files (enables file logging) | None (stdout only) | `/var/log/routini` |
| `LOG_JSON` | Enable JSON format | `false` | `1` or `true` |
| `NO_COLOR` | Disable ANSI colors | Not set (colors enabled) | `1` |

## Log Levels

From most verbose to least verbose:
- **TRACE**: Very detailed, per-request information
- **DEBUG**: Detailed information for debugging
- **INFO**: General informational messages (default)
- **WARN**: Warning messages
- **ERROR**: Error messages

## Module-Specific Filtering

### Common Patterns

```bash
# Everything at INFO, routini at DEBUG
RUST_LOG=info,routini=debug

# Routini at DEBUG, Pingora at INFO
RUST_LOG=routini=debug,pingora=info

# Specific module at TRACE
RUST_LOG=info,routini::proxy=trace

# Multiple specific modules
RUST_LOG=routini::proxy=debug,routini::load_balancing=trace,pingora::lb=info

# Everything at DEBUG except Pingora
RUST_LOG=debug,pingora=info
```

### Useful Debug Configurations

**Debugging routing issues:**
```bash
RUST_LOG=routini::proxy=debug,routini::server_builder=debug
```

**Debugging load balancing:**
```bash
RUST_LOG=routini::load_balancing=debug,routini::adaptive_loadbalancer=debug
```

**Debugging backend health:**
```bash
RUST_LOG=routini::load_balancing::health_check=debug
```

**Everything verbose (very noisy):**
```bash
RUST_LOG=trace
```

## File Logging

### Daily Rotation

When `LOG_DIR` is set, logs are written to files with daily rotation:

```
/var/log/routini/
├── routini.log              # Current day
├── routini.log.2025-01-14   # Previous days
├── routini.log.2025-01-13
└── routini.log.2025-01-12
```

### File Output Features

- **Non-blocking writes**: Uses background thread for file I/O
- **Automatic rotation**: New file created at midnight UTC
- **Thread information**: Thread IDs and names included
- **Module targets**: Shows which module emitted each log

### Dual Output

When file logging is enabled:
- **Files**: Get detailed logs with timestamps, thread info, and targets
- **stdout**: Gets simplified logs for console monitoring

Both outputs respect the same `RUST_LOG` filter.

## JSON Format

Enable with `LOG_JSON=1` for structured logging:

```json
{
  "timestamp": "2025-01-15T10:30:45.123456Z",
  "level": "INFO",
  "target": "routini::proxy",
  "thread_id": "ThreadId(42)",
  "thread_name": "tokio-runtime-worker",
  "message": "Request routed to backend",
  "fields": {
    "backend": "127.0.0.1:8080",
    "route": "/api/*",
    "latency_ms": 15.3
  }
}
```

### When to Use JSON

✅ **Use JSON when:**
- Shipping logs to ELK, Splunk, Loki, etc.
- Using log aggregation tools
- Need machine-parsable logs
- Running in containers/Kubernetes

❌ **Use plain format when:**
- Local development
- Debugging on server via SSH
- Need human-readable output

## Docker/Container Logging

### Dockerfile Example

```dockerfile
FROM rust:1.75 as builder
WORKDIR /app
COPY . .
RUN cargo build --release

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y ca-certificates && rm -rf /var/lib/apt/lists/*

# Create log directory
RUN mkdir -p /var/log/routini && chmod 755 /var/log/routini

COPY --from=builder /app/target/release/routini /usr/local/bin/

# Set production logging defaults
ENV RUST_LOG=info,routini=debug,pingora=info
ENV LOG_DIR=/var/log/routini
ENV LOG_JSON=1
ENV NO_COLOR=1

EXPOSE 3500 5000 9090
CMD ["routini"]
```

### Docker Compose

```yaml
version: '3'
services:
  routini:
    build: .
    ports:
      - "3500:3500"  # Main proxy
      - "5000:5000"  # Strategy endpoint
      - "9090:9090"  # Prometheus
    environment:
      - RUST_LOG=info,routini=debug
      - LOG_DIR=/var/log/routini
      - LOG_JSON=1
    volumes:
      - ./logs:/var/log/routini  # Persist logs
```

## Log Examples

### Startup Logs
```
2025-01-15T10:30:45.123Z INFO routini: Starting Routini reverse proxy
2025-01-15T10:30:45.124Z DEBUG routini::server_builder: Adding route: /api/*
2025-01-15T10:30:45.125Z INFO routini::load_balancing: Initialized 40 backends
2025-01-15T10:30:45.126Z INFO pingora::server: Listening on 127.0.0.1:3500
```

### Request Logs
```
2025-01-15T10:30:46.234Z DEBUG routini::proxy: Routing request path=/api/users
2025-01-15T10:30:46.235Z DEBUG routini::load_balancing: Selected backend 127.0.0.1:4001
2025-01-15T10:30:46.250Z INFO routini::proxy: Request completed latency_ms=15.3 status=200
```

### Health Check Logs
```
2025-01-15T10:30:47.000Z DEBUG routini::load_balancing::health_check: Running health checks
2025-01-15T10:30:47.015Z WARN routini::load_balancing::health_check: Backend unhealthy backend=127.0.0.1:4005
2025-01-15T10:30:47.016Z INFO routini::load_balancing: Active backends: 39/40
```

### Strategy Change Logs
```
2025-01-15T10:31:00.000Z INFO routini::adaptive_loadbalancer::decision_engine: Evaluating strategy
2025-01-15T10:31:00.001Z INFO routini::adaptive_loadbalancer: Strategy updated to FewestConnections for /api/*
```

### Error Logs
```
2025-01-15T10:31:15.500Z ERROR routini::proxy: Failed to connect to backend backend=127.0.0.1:4010 error="Connection refused"
2025-01-15T10:31:15.501Z WARN routini::load_balancing: No healthy backends available route=/api/*
```

## Performance Considerations

### Log Level Impact

- **TRACE**: ~10-20% overhead, use only for specific debugging
- **DEBUG**: ~5-10% overhead, acceptable for production with filtering
- **INFO**: ~1-2% overhead, recommended for production
- **WARN/ERROR**: Minimal overhead

### Best Practices

1. **Use module filtering**: Don't enable DEBUG globally
   ```bash
   # Good: Only debug specific modules
   RUST_LOG=info,routini::proxy=debug
   
   # Bad: Everything at debug
   RUST_LOG=debug
   ```

2. **Use structured fields**: Add context to logs
   ```rust
   tracing::info!(
       backend = %backend.addr,
       latency_ms = latency.as_millis(),
       "Request completed"
   );
   ```

3. **Avoid expensive computations in logs**:
   ```rust
   // Bad: Always computes even if not logged
   tracing::debug!("Backends: {}", expensive_format());
   
   // Good: Only computes if debug enabled
   tracing::debug!("Backends: {}", tracing::field::debug(backends));
   ```

## Troubleshooting

### No logs appearing

**Check log level:**
```bash
RUST_LOG=debug cargo run
```

### File permissions error

```bash
sudo mkdir -p /var/log/routini
sudo chown $USER:$USER /var/log/routini
```

### Too many logs

**Reduce verbosity:**
```bash
RUST_LOG=warn cargo run
```

### Can't find specific issue

**Enable trace for one module:**
```bash
RUST_LOG=info,routini::proxy=trace cargo run
```

## Integration with Log Aggregation

### Fluentd

```conf
<source>
  @type tail
  path /var/log/routini/routini.log
  pos_file /var/log/td-agent/routini.log.pos
  tag routini
  format json
  time_key timestamp
  time_format %Y-%m-%dT%H:%M:%S.%NZ
</source>
```

### Promtail (Loki)

```yaml
scrape_configs:
  - job_name: routini
    static_configs:
      - targets:
          - localhost
        labels:
          job: routini
          __path__: /var/log/routini/*.log
    pipeline_stages:
      - json:
          expressions:
            level: level
            timestamp: timestamp
            message: message
```

### Filebeat (Elasticsearch)

```yaml
filebeat.inputs:
  - type: log
    enabled: true
    paths:
      - /var/log/routini/*.log
    json.keys_under_root: true
    json.add_error_key: true
```

## Further Reading

- [tracing documentation](https://docs.rs/tracing/)
- [tracing-subscriber](https://docs.rs/tracing-subscriber/)
- [RUST_LOG syntax](https://docs.rs/env_logger/#enabling-logging)
