#!/bin/bash
# Routini Logging Examples
# This script demonstrates different logging configurations

set -e

echo "=== Routini Logging Demonstrations ==="
echo ""

# Create log directory if needed
mkdir -p /tmp/routini-logs

echo "1. Basic INFO logging (default)"
echo "   Command: cargo run"
echo "   Press Ctrl+C after a few seconds..."
echo ""
RUST_LOG=info cargo run &
PID=$!
sleep 5
kill $PID 2>/dev/null || true
wait $PID 2>/dev/null || true
echo ""

echo "2. DEBUG level for routini, INFO for pingora"
echo "   Command: RUST_LOG=routini=debug,pingora=info cargo run"
echo "   Press Ctrl+C after a few seconds..."
echo ""
RUST_LOG=routini=debug,pingora=info cargo run &
PID=$!
sleep 5
kill $PID 2>/dev/null || true
wait $PID 2>/dev/null || true
echo ""

echo "3. File logging with daily rotation"
echo "   Command: LOG_DIR=/tmp/routini-logs cargo run"
echo "   Logs will be written to: /tmp/routini-logs/routini.log"
echo "   Press Ctrl+C after a few seconds..."
echo ""
LOG_DIR=/tmp/routini-logs cargo run &
PID=$!
sleep 5
kill $PID 2>/dev/null || true
wait $PID 2>/dev/null || true
echo ""
echo "   Log file contents:"
head -n 20 /tmp/routini-logs/routini.log 2>/dev/null || echo "   (no logs yet)"
echo ""

echo "4. JSON formatted logs for log aggregation"
echo "   Command: LOG_DIR=/tmp/routini-logs LOG_JSON=1 cargo run"
echo "   Press Ctrl+C after a few seconds..."
echo ""
LOG_DIR=/tmp/routini-logs LOG_JSON=1 cargo run &
PID=$!
sleep 5
kill $PID 2>/dev/null || true
wait $PID 2>/dev/null || true
echo ""
echo "   JSON log sample (pretty printed):"
tail -n 1 /tmp/routini-logs/routini.log 2>/dev/null | jq '.' 2>/dev/null || echo "   (jq not installed for pretty printing)"
echo ""

echo "5. Trace-level debugging for specific module"
echo "   Command: RUST_LOG=info,routini::proxy=trace cargo run"
echo "   Very verbose! Press Ctrl+C quickly..."
echo ""
RUST_LOG=info,routini::proxy=trace cargo run &
PID=$!
sleep 3
kill $PID 2>/dev/null || true
wait $PID 2>/dev/null || true
echo ""

echo "6. Production configuration (file + JSON + filtered)"
echo "   Command: RUST_LOG=info,routini=debug LOG_DIR=/tmp/routini-logs LOG_JSON=1 NO_COLOR=1 cargo run"
echo "   Press Ctrl+C after a few seconds..."
echo ""
RUST_LOG=info,routini=debug LOG_DIR=/tmp/routini-logs LOG_JSON=1 NO_COLOR=1 cargo run &
PID=$!
sleep 5
kill $PID 2>/dev/null || true
wait $PID 2>/dev/null || true
echo ""

echo "=== Demo Complete ==="
echo ""
echo "Log files are in: /tmp/routini-logs/"
ls -lh /tmp/routini-logs/ 2>/dev/null || true
echo ""
echo "Clean up with: rm -rf /tmp/routini-logs"
