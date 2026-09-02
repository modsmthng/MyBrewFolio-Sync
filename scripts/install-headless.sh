#!/usr/bin/env sh
# SPDX-License-Identifier: GPL-3.0-or-later

# Installs a self-contained Docker Compose setup without ever asking for an
# OAuth client secret. The official image contains its public OAuth client ID.

set -eu

INSTALL_DIR="${MYBREWFOLIO_SYNC_HOME:-${HOME:?HOME must be set}/.config/mybrewfolio-sync}"
MACHINE_HOST=""
START_DAEMON=1
NON_INTERACTIVE=0
UPDATE_HELPER=0

usage() {
  printf '%s\n' "Usage: install-headless.sh [--host HOST] [--no-start] [--non-interactive] | --update-helper"
}

while [ "$#" -gt 0 ]; do
  case "$1" in
    --update-helper)
      UPDATE_HELPER=1
      shift
      ;;
    --host)
      [ "$#" -ge 2 ] || { usage >&2; exit 64; }
      MACHINE_HOST="$2"
      shift 2
      ;;
    --no-start)
      START_DAEMON=0
      shift
      ;;
    --non-interactive)
      NON_INTERACTIVE=1
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

write_helper() {
  helper_tmp=$(mktemp "$INSTALL_DIR/.sync-helper.XXXXXX")
  cat > "$helper_tmp" <<'EOF'
#!/usr/bin/env sh
set -eu

INSTALL_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
if [ "$#" -eq 0 ]; then
  set -- help
fi
case "$1:${2-}" in
  help:*|--help:*|-h:*|*:help|*:--help|*:-h)
    exec docker compose --project-directory "$INSTALL_DIR" -f "$INSTALL_DIR/compose.yaml" run --rm --no-deps sync "$@"
    ;;
  notes:enable)
    if [ "$#" -ne 2 ]; then
      printf '%s\n' 'Usage: sync notes enable' >&2
      exit 64
    fi
    if [ ! -t 0 ] || [ ! -t 1 ] || [ ! -t 2 ]; then
      printf '%s\n' 'notes enable requires an interactive terminal. Use notes activate-preview and notes activate <backup-id> <decisions.json> --confirm for scripted setup.' >&2
      exit 64
    fi
    # Compose exec allocates a terminal by default; only scripted commands use -T.
    exec docker compose --project-directory "$INSTALL_DIR" -f "$INSTALL_DIR/compose.yaml" exec -e "MYBREWFOLIO_SYNC_CLI_INSTALL_DIR=$INSTALL_DIR" sync mybrewfolio-syncd "$@"
    ;;
  *)
    exec docker compose --project-directory "$INSTALL_DIR" -f "$INSTALL_DIR/compose.yaml" exec -T sync mybrewfolio-syncd "$@"
    ;;
esac
EOF
  chmod 700 "$helper_tmp"
  mv -f "$helper_tmp" "$INSTALL_DIR/sync"
}

if [ "$UPDATE_HELPER" -eq 1 ]; then
  if [ -n "$MACHINE_HOST" ] || [ "$START_DAEMON" -ne 1 ] || [ "$NON_INTERACTIVE" -ne 0 ]; then
    printf '%s\n' '--update-helper cannot be combined with installation options.' >&2
    exit 64
  fi
  if [ ! -f "$INSTALL_DIR/compose.yaml" ] || [ ! -f "$INSTALL_DIR/sync" ] || [ -L "$INSTALL_DIR/sync" ]; then
    printf '%s\n' 'No existing installation with a regular helper was found. Set MYBREWFOLIO_SYNC_HOME if you used a custom installation directory.' >&2
    exit 73
  fi
  write_helper
  printf '%s\n' "Updated helper: $INSTALL_DIR/sync" 'Configuration, credentials, keys, volumes, and running containers were not changed. Update the Docker image separately before using notes enable.'
  exit 0
fi

TTY_AVAILABLE=0
if ( : </dev/tty ) 2>/dev/null; then
  TTY_AVAILABLE=1
fi

read_from_tty() {
  printf '%s' "$1" >/dev/tty
  IFS= read -r value </dev/tty || return 1
  printf '%s' "$value"
}

if [ -z "$MACHINE_HOST" ]; then
  if [ "$NON_INTERACTIVE" -eq 1 ]; then
    printf '%s\n' "--host is required together with --non-interactive." >&2
    exit 64
  fi
  if [ "$TTY_AVAILABLE" -ne 1 ]; then
    printf '%s\n' "No terminal is available. Use --host HOST --non-interactive for automation." >&2
    exit 64
  fi
  machine_answer=$(read_from_tty "GaggiMate host [gaggimate.local]: ") || {
    printf '%s\n' "Could not read the GaggiMate host from the terminal." >&2
    exit 74
  }
  MACHINE_HOST=${machine_answer:-gaggimate.local}
fi

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
chmod 700 "$INSTALL_DIR"
KEY_FILE="$INSTALL_DIR/state.key"
COMPOSE_FILE="$INSTALL_DIR/compose.yaml"
ENV_FILE="$INSTALL_DIR/.env"
HELPER_FILE="$INSTALL_DIR/sync"

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

write_helper
chmod 600 "$ENV_FILE" "$COMPOSE_FILE"
chmod 700 "$HELPER_FILE"

printf '%s\n' "Created MyBrewFolio Sync configuration in $INSTALL_DIR"
printf '%s\n' "Local command: $HELPER_FILE help"

if [ "$START_DAEMON" -eq 0 ]; then
  printf '%s\n' "The daemon was not started (--no-start). Start the Compose service when you are ready."
  exit 0
fi

docker compose --project-directory "$INSTALL_DIR" -f "$COMPOSE_FILE" up -d

if [ "$NON_INTERACTIVE" -eq 1 ]; then
  printf '%s\n' "Installation is ready. Pair later with: $HELPER_FILE auth begin"
  exit 0
fi

if [ "$TTY_AVAILABLE" -ne 1 ]; then
  printf '%s\n' "Installation is ready. Pair later with: $HELPER_FILE auth begin"
  exit 0
fi

pair_answer=$(read_from_tty "Connect your MyBrewFolio account now? [Y/n]: ") || {
  printf '%s\n' "Could not read the pairing choice. Pair later with: $HELPER_FILE auth begin" >&2
  exit 0
}
case "$pair_answer" in
  n|N|no|NO|No)
    printf '%s\n' "Installation is ready. Pair later with: $HELPER_FILE auth begin"
    ;;
  *)
    printf '%s\n' "Open the authorization URL below in your browser, then finish sign-in."
    if ! "$HELPER_FILE" auth begin; then
      printf '%s\n' "Could not begin pairing. Try again later with: $HELPER_FILE auth begin" >&2
      exit 1
    fi
    printf '%s\n' "Waiting for browser authorization (up to 10 minutes)..."
    if ! "$HELPER_FILE" auth wait; then
      printf '%s\n' "Pairing was not completed. The installation is intact; retry with: $HELPER_FILE auth begin" >&2
      exit 1
    fi
    printf '%s\n' "MyBrewFolio Sync is connected. Check it any time with: $HELPER_FILE diagnose"
    ;;
esac
