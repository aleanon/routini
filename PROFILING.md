# Profiling Guide

## Using Flamegraph

### 1. Build in release mode with debug symbols

First, ensure your `Cargo.toml` has debug symbols enabled for release builds:

```toml
[profile.release]
debug = true
```

### 2. Set up perf permissions (Linux only)

You need to allow perf to capture events:

```bash
# Temporarily (until reboot)
echo -1 | sudo tee /proc/sys/kernel/perf_event_paranoid

# Or permanently, add to /etc/sysctl.conf:
# kernel.perf_event_paranoid = -1
```

### 3. Run flamegraph on routini

```bash
cd routini

# Build and run with flamegraph (uses release profile by default)
sudo cargo flamegraph --bin routini

# Use a specific profile (e.g., release with debug symbols)
sudo cargo flamegraph --profile release --bin routini

# Or use a custom profile defined in Cargo.toml
sudo cargo flamegraph --profile profiling --bin routini

# In another terminal, run your load test
k6 run test_throughput.js
# Or use a simpler constant load test like:
# wrk -t4 -c100 -d30s http://localhost:3500/auth/health
```

Press Ctrl+C after running your load test to stop the profiling.

This will generate `flamegraph.svg` in your current directory.

### 4. Analyzing the flamegraph

Open the SVG in a browser:
```bash
firefox flamegraph.svg
# or
google-chrome flamegraph.svg
```

**How to read it:**
- The X-axis shows the sample population (wider = more CPU time)
- The Y-axis shows stack depth (call stack)
- Each box is a function in the call stack
- Click on boxes to zoom in
- Hover to see function names and percentages
- Look for wide boxes in the hot path - those are your bottlenecks

### 5. Alternative: Using perf directly

For more control:

```bash
# Record
sudo perf record -F 99 -g --call-graph dwarf -- ./target/release/routini

# In another terminal, run load test
k6 run test_throughput.js

# Stop with Ctrl+C, then generate flamegraph
perf script | inferno-collapse-perf | inferno-flamegraph > flamegraph.svg
```

### 6. Compare plain vs routini

To compare both implementations:

```bash
# Profile plain
cd plain
sudo cargo flamegraph --bin plain
# Run load test, stop, get flamegraph-plain.svg
mv flamegraph.svg flamegraph-plain.svg

# Profile routini
cd ../routini
sudo cargo flamegraph --bin routini
# Run load test, stop, get flamegraph-routini.svg
mv flamegraph.svg flamegraph-routini.svg
```

Compare the two SVGs to see where routini spends more time.

## What to look for

In the flamegraph, look for:
1. **Router overhead** - `matchit::Router::at` or path matching functions
2. **Arc operations** - `Arc::clone`, atomic operations
3. **Metrics recording** - Any atomic increment/decrement operations
4. **Context allocation** - Memory allocation in `new_ctx`
5. **Lock contention** - Any `RwLock` or `Mutex` operations (though you said this isn't in hot path)
6. **Path manipulation** - `set_raw_path` operations

The widest functions in the hot path are your biggest optimization opportunities.
