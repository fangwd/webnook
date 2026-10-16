#!/usr/bin/env bash
# Measure idle and loaded memory and latency of a release example.
#
#   scripts/measure.sh <example> [features] [path]
#   scripts/measure.sh embedded embed /
#   scripts/measure.sh startup_site sqlite /
#
# Load generation uses `oha` (cargo install oha) when installed. Set
# DURATION (default 30s) and CONNECTIONS (default 32) to change the load.
# Run on the reference host (1 CPU, 512 MB) and record the full output.
set -euo pipefail

example=${1:?usage: measure.sh <example> [features] [path]}
features=${2:-}
path=${3:-/}
duration=${DURATION:-30s}
connections=${CONNECTIONS:-32}
url="http://127.0.0.1:3000${path}"

cd "$(dirname "$0")/.."
cargo build --release --locked --example "$example" --features "$features" >&2
bin="target/release/examples/$example"
workdir=$(mktemp -d)
trap 'kill "$pid" 2>/dev/null || true; rm -rf "$workdir"' EXIT

rss_kib() { ps -o rss= -p "$1" | tr -d ' '; }

echo "== build"
echo "example:     $example"
echo "features:    ${features:-none}"
echo "commit:      $(git rev-parse --short HEAD 2>/dev/null || echo unknown)"
echo "rustc:       $(rustc --version)"
echo "target:      $(rustc -vV | sed -n 's/^host: //p')"
echo "binary:      $(wc -c < "$bin" | tr -d ' ') bytes"
echo "cpus:        $(getconf _NPROCESSORS_ONLN)"
echo "assets:      $(du -sk examples/assets | cut -f1) KiB in examples/assets"

DATABASE_PATH="$workdir/site.db" "$bin" > "$workdir/log.txt" 2>&1 &
pid=$!
for _ in $(seq 50); do
  curl -fs -o /dev/null "$url" && break
  sleep 0.1
done

sleep 2
echo "== idle"
echo "rss:         $(rss_kib "$pid") KiB"

if command -v oha > /dev/null; then
  echo "== load: $connections connections for $duration on $path"
  oha --no-tui -z "$duration" -c "$connections" "$url" | sed -n '/Summary/,/Status code distribution/p'
  echo "rss after:   $(rss_kib "$pid") KiB"
  sleep 5
  echo "rss settled: $(rss_kib "$pid") KiB"
else
  echo "== load: skipped (install oha to measure under load)"
fi

kill -TERM "$pid"
wait "$pid" || true
