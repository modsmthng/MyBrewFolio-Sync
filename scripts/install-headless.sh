#!/usr/bin/env sh
# SPDX-License-Identifier: GPL-3.0-or-later

# Installs a self-contained Docker Compose setup without ever asking for an
# OAuth client secret. The official image contains its public OAuth client ID.

set -eu

INSTALL_DIR="${MYBREWFOLIO_SYNC_HOME:-${HOME:?HOME must be set}/.config/mybrewfolio-sync}"
MACHINE_HOST="gaggimate.local"
START_DAEMON=1

usage() {
  printf '%s\n' "Usage: install-headless.sh [--host HOST] [--no-start]"
}

while [ "$#" -gt 0 ]; do
  case "$1" in
    --host)
      [ "$#" -ge 2 ] || { usage >&2; exit 64; }
      MACHINE_HOST="$2"
      shift 2
      ;;
    --no-start)
      START_DAEMON=0
      shift
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      usage >&2
      exit 64
      ;;
  esac
done

# The value is written to a Compose .env file. Keep it to hostnames, IPv4/IPv6
# literals, and optional ports so command-line input cannot add Compose entries.
case "$MACHINE_HOST" in
  ""|*[!A-Za-z0-9._:-]*)
    printf '%s\n' "--host must be a hostname, IP address, or host:port." >&2
    exit 64
    ;;
esac

command -v docker >/dev/null 2>&1 || {
  printf '%s\n' "Docker with the Compose plugin is required." >&2
  exit 69
}
docker compose version >/dev/null 2>&1 || {
  printf '%s\n' "Docker Compose v2 is required." >&2
  exit 69
}

umask 077
mkdir -p "$INSTALL_DIR"
KEY_FILE="$INSTALL_DIR/state.key"
COMPOSE_FILE="$INSTALL_DIR/compose.yaml"
ENV_FILE="$INSTALL_DIR/.env"

if [ -e "$COMPOSE_FILE" ]; then
  printf '%s\n' "Refusing to overwrite existing configuration: $COMPOSE_FILE" >&2
  printf '%s\n' "Edit it manually or choose MYBREWFOLIO_SYNC_HOME with an empty directory." >&2
  exit 73
fi

if [ ! -f "$KEY_FILE" ]; then
  command -v openssl >/dev/null 2>&1 || {
    printf '%s\n' "OpenSSL is required to create the local state key." >&2
    exit 69
  }
  openssl rand -base64 32 > "$KEY_FILE"
  chmod 600 "$KEY_FILE"
  printf '%s\n' "Created local encryption key: $KEY_FILE"
fi

cat > "$ENV_FILE" <<EOF
MYBREWFOLIO_SYNC_GAGGIMATE_HOST=$MACHINE_HOST
MYBREWFOLIO_SYNC_STATE_KEY=$KEY_FILE
EOF

cat > "$COMPOSE_FILE" <<'EOF'
services:
  sync:
    image: ghcr.io/modsmthng/mybrewfolio-sync:latest
    restart: unless-stopped
    environment:
      MYBREWFOLIO_SYNC_GAGGIMATE_HOST: ${MYBREWFOLIO_SYNC_GAGGIMATE_HOST}
      MYBREWFOLIO_SYNC_CREDENTIAL_KEY_FILE: /run/secrets/mybrewfolio_sync_state_key
    secrets:
      - mybrewfolio_sync_state_key
    volumes:
      - sync-data:/data
    healthcheck:
      test: ["CMD", "mybrewfolio-syncd", "health"]
      interval: 30s
      timeout: 5s
      retries: 3

secrets:
  mybrewfolio_sync_state_key:
    file: ${MYBREWFOLIO_SYNC_STATE_KEY}

volumes:
  sync-data:
EOF
chmod 600 "$ENV_FILE" "$COMPOSE_FILE"

printf '%s\n' "Created MyBrewFolio Sync configuration in $INSTALL_DIR"
if [ "$START_DAEMON" -eq 1 ]; then
  docker compose --project-directory "$INSTALL_DIR" -f "$COMPOSE_FILE" up -d
fi

printf '%s\n' "Connect your account once with:"
printf '%s\n' "  docker compose --project-directory '$INSTALL_DIR' -f '$COMPOSE_FILE' exec sync mybrewfolio-syncd auth begin"
printf '%s\n' "  docker compose --project-directory '$INSTALL_DIR' -f '$COMPOSE_FILE' exec sync mybrewfolio-syncd auth wait"
