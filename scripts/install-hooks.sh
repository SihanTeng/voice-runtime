#!/bin/sh
set -eu
git config --local core.hooksPath .githooks
echo 'Installed staged-snapshot pre-commit hook.'
