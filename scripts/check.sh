#!/bin/sh
set -eu
sh scripts/rust-check.sh
python3 scripts/test_hooks.py
python3 scripts/test_dev.py
