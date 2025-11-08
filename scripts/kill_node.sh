#!/usr/bin/env bash

# Exit immediately if a command exits with a non-zero status.
set -e

# Check if lsof is installed
if ! command -v lsof &> /dev/null; then
  echo "Error: 'lsof' command not found. Please install it to use this script." >&2
  exit 1
fi

# Check if a port number was provided
if [ "$#" -lt 1 ]; then
  echo "Usage: $0 <port>" >&2
  echo "Example: $0 7001" >&2
  exit 1
fi

PORT="$1"

# Find the PID listening on the specified port.
# -iTCP:${PORT} : Find processes with TCP on this port.
# -sTCP:LISTEN : Filter for processes in the LISTEN state.
# -n           : Do not resolve hostnames (faster).
# -P           : Do not resolve port names (faster).
# -t           : Output *only* the Process ID (PID).
PID=$(lsof -iTCP:"${PORT}" -sTCP:LISTEN -n -P -t | head -n 1)

if [ -z "$PID" ]; then
  echo "No process found listening on port ${PORT}."
else
  echo "Found process $PID on port ${PORT}. Sending SIGTERM (kill)..."
  # Kill the process
  kill "$PID"
  echo "Process $PID killed."
fi