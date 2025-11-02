#!/usr/bin/env bash

printf 'LIST_FILES\n' | nc -q 0 127.0.0.1 7000
