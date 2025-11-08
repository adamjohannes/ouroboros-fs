#!/usr/bin/env bash

# Detect OS to set correct netcat options
# Linux (Arch) uses `nc -q 0`
# macOS/BSD uses `nc -w 1`
NC_OPTS="-q 0" # Default for Linux

# Detect OS to set correct wc (word count) options
# Linux (Arch) uses GNU `wc --bytes`
# macOS/BSD uses `wc -c`
WC_OPTS="--bytes" # Default for Linux

if [[ "$(uname -s)" == "Darwin" ]]; then
  NC_OPTS="-w 1"
  WC_OPTS="-c"
fi

# Check if the user passed a path
if [ "$#" -lt 1 ]; then
  echo "A file path is required to run the script"
  exit 1
fi

# Check if the path is valid
if [ ! -e "$1" ]; then
  echo "Invalid file path provided"
  exit 1
fi

FILE="$1";

# Get the byte count from wc and pipe to `xargs` to trim whitespace (for BSD wc)
SIZE_STR=$(wc ${WC_OPTS} < "${FILE}" | xargs)

# Build message header and body, then send it to a node using netcat
( printf "FILE PUSH ${SIZE_STR} ${FILE}\n"; cat "${FILE}" ) | nc ${NC_OPTS} 127.0.0.1 7000