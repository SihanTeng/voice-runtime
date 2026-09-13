#!/bin/sh
# Resolve from the script, so invocation also works outside the repository.
set -eu
voice_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
if ! command -v python3 >/dev/null 2>&1; then
    echo 'Python 3.9+ is required for the development launcher.' >&2
    case "$(uname -s)" in
        Darwin) echo 'Install Apple Command Line Tools: xcode-select --install; then install Python 3.9+ if needed.' >&2 ;;
        Linux) echo 'Ubuntu/WSL: sudo apt-get update && sudo apt-get install python3 build-essential curl ca-certificates git' >&2 ;;
        *) echo 'Use macOS, Linux, or Windows through WSL2; see docs/SETUP.md.' >&2 ;;
    esac
    exit 1
fi
exec python3 "$voice_root/scripts/dev.py" "$@"
