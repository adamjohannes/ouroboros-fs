#!/usr/bin/env bash

# Detect OS to set correct netcat options
# Linux (Arch) uses `nc -q 0` to close connection after EOF
# macOS/BSD uses `nc -c` for the same behavior
NC_OPTS="-q 0" # Default for Linux
if [[ "$(uname -s)" == "Darwin" ]]; then
  NC_OPTS="-c"
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
# The server responds with the raw file bytes, which
# are printed directly to stdout.
printf "FILE PULL ${FILE_NAME}\n" | nc ${NC_OPTS} 127.0.0.1 7000