#!/bin/sh
# SPDX-License-Identifier: GPL-3.0-or-later
set -eu
script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
node "$script_dir/generate-icons.mjs"
