#!/usr/bin/env bash

# Check if the used passed a path
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

# Build message header and body, then send it to a node using netcat 
( printf "PUSH_FILE $(wc --bytes < "${FILE}") ${FILE}\n"; cat "${FILE}" ) | nc -q 0 127.0.0.1 7000

