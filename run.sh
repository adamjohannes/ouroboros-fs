#!/usr/bin/env bash
set -Eeuo pipefail

# Build the project
cargo build --quiet

pids=()

# Start the nodes
./target/debug/rust_socket_server 7001 & pids+=($!)
./target/debug/rust_socket_server 7002 & pids+=($!)
./target/debug/rust_socket_server 7003 & pids+=($!)

cleanup() {
  if ((${#pids[@]})); then
    kill "${pids[@]}" 2>/dev/null || true
    wait "${pids[@]}" 2>/dev/null || true
  fi
}
trap cleanup EXIT INT TERM

# Set the "next_node" field on all running nodes
printf 'SET_NEXT 127.0.0.1:7002\n' | nc -N 127.0.0.1 7001
printf 'SET_NEXT 127.0.0.1:7003\n' | nc -N 127.0.0.1 7002
printf 'SET_NEXT 127.0.0.1:7001\n' | nc -N 127.0.0.1 7003

# Block until user types 'quit'
while IFS= read -r -p "Type 'quit' to exit: " input; do
  [[ "$input" == "quit" ]] && break
done
