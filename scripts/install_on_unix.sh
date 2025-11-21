#!/bin/bash

set -e

cargo build --release

readonly SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
readonly PROJECT_ROOT="$(dirname "$SCRIPT_DIR")"

cp ${PROJECT_ROOT}/target/release/docserver ~/.local/bin
