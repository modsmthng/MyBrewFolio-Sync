#!/bin/sh
# SPDX-License-Identifier: GPL-3.0-or-later
set -eu
# Supervisor owns /data. Only initialize our dedicated state directory, leaving
# options.json and other Supervisor-managed files untouched. Never recurse.
if [ "$(id -u)" = 0 ]; then
  mkdir -p /data/state
  chown 10001:10001 /data/state
  chmod 700 /data/state
  exec setpriv --reuid=10001 --regid=10001 --init-groups -- mybrewfolio-syncd "$@"
fi
exec mybrewfolio-syncd "$@"
