#!/usr/bin/env bash

# Detect OS to set correct netcat options
# Linux (Arch) uses `nc -q 0`
# macOS/BSD uses `nc -w 1`
NC_OPTS="-q 0" # Default for Linux
if [[ "$(uname -s)" == "Darwin" ]]; then
  NC_OPTS="-w 1"
fi

# Check if a file name was provided
if [ "$#" -lt 1 ]; then
  echo "Usage: $0 <file_name_on_server>" >&2
  echo "Outputs the file to stdout. Redirect to save it." >&2
  echo "Example: $0 Cargo.toml > ./downloaded_cargo.toml" >&2
  exit 1
fi

FILE_NAME="$1";

# Send the FILE PULL command.
printf "FILE PULL ${FILE_NAME}\n" | nc ${NC_OPTS} 127.0.0.1 7000