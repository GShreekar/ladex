#!/usr/bin/env bash
# Runs quinn and iroh transfers alternately, so background load hits both alike, and checks every received file.
# Same machine:   spikes/transport/bench.sh <work-dir> [runs] [size]
# Needs: cargo, jq.
set -euo pipefail

work=${1:?usage: bench.sh <work-dir> [runs] [size]}
runs=${2:-5}
size=${3:-4GiB}
here=$(cd "$(dirname "$0")" && pwd)
bin="$here/target/release/transport-spike"
results="$work/results.jsonl"

# A failed run would otherwise leave its sender waiting forever for a receiver.
trap 'jobs -p | xargs -r kill 2>/dev/null' EXIT

cargo build --release --quiet --manifest-path "$here/Cargo.toml"
mkdir -p "$work"
if [[ ! -f "$work/source.bin" ]]; then
    echo "generating $size test file"
    "$bin" gen "$work/source.bin" --size "$size"
fi
expected=$("$bin" hash "$work/source.bin")

wait_for_ticket() {
    for _ in $(seq 100); do
        [[ -f "$work/ticket" ]] && return 0
        sleep 0.1
    done
    echo "the sender never published a ticket" >&2
    return 1
}

clean_run_files() {
    rm -f "$work/ticket" "$work/received.bin" "$work/send.json" "$work/recv.json"
    rm -rf "$work/iroh-send-store" "$work/iroh-recv-store"
}

record() {
    local run=$1
    cat "$work/send.json" "$work/recv.json" | jq -c --argjson run "$run" '. + {run: $run}' >>"$results"
    jq -r '"  \(.transport) \(.role): \(.transfer_mib_per_sec | floor) MiB/s, rss \(.max_rss_mib | floor) MiB"' "$work/send.json" "$work/recv.json"
}

verify() {
    local actual
    actual=$("$bin" hash "$work/received.bin")
    if [[ "$actual" != "$expected" ]]; then
        echo "received file hash $actual does not match $expected" >&2
        exit 1
    fi
}

run_quinn() {
    "$bin" quinn-send "$work/source.bin" --ticket-file "$work/ticket" >"$work/send.json" &
    local sender=$!
    wait_for_ticket
    "$bin" quinn-recv "$(cat "$work/ticket")" "$work/received.bin" >"$work/recv.json"
    wait "$sender"
}

run_iroh() {
    "$bin" iroh-send "$work/source.bin" "$work/iroh-send-store" --ticket-file "$work/ticket" >"$work/send.json" &
    local sender=$!
    wait_for_ticket
    "$bin" iroh-recv "$(cat "$work/ticket")" "$work/received.bin" "$work/iroh-recv-store" >"$work/recv.json"
    wait "$sender"
}

for run in $(seq "$runs"); do
    for transport in quinn iroh; do
        clean_run_files
        echo "run $run/$runs: $transport"
        "run_$transport" 2>/dev/null
        verify
        record "$run"
    done
done
clean_run_files

echo
echo "medians over $runs runs (from $results):"
jq -rs '
    def median: sort | .[length / 2 | floor];
    group_by([.transport, .role])[]
    | [.[0].transport, .[0].role,
       (map(.transfer_mib_per_sec) | median | floor | tostring) + " MiB/s",
       "prepare " + (map(.prepare_secs) | median * 10 | round / 10 | tostring) + "s",
       "finalize " + (map(.finalize_secs) | median * 10 | round / 10 | tostring) + "s",
       "cpu " + (map(.cpu_user_secs + .cpu_system_secs) | median * 10 | round / 10 | tostring) + "s",
       "rss " + (map(.max_rss_mib) | median | floor | tostring) + " MiB"]
    | @tsv' "$results" | column -t -s $'\t'
