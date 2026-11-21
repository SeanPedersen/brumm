#!/usr/bin/env bash
# Builds the brumm web client (trunk, release) and serves it with deploy/docker-compose.yml on a
# Docker host over SSH. Host-specific settings live in deploy/.env (see .env.example).
set -euo pipefail

DEPLOY_DIR_LOCAL="$(cd "$(dirname "$0")" && pwd)"
REPO_DIR="$(dirname "$DEPLOY_DIR_LOCAL")"
DIST_DIR="$REPO_DIR/target/web-dist"
ENV_FILE="$DEPLOY_DIR_LOCAL/.env"

[[ -f "$ENV_FILE" ]] || { echo "missing $ENV_FILE (copy .env.example)" >&2; exit 1; }
set -a; source "$ENV_FILE"; set +a
: "${DEPLOY_HOST:?set DEPLOY_HOST in .env}" "${DEPLOY_DIR:?set DEPLOY_DIR in .env}"
: "${EDGE_NETWORK:?set EDGE_NETWORK in .env}"

echo "→ Building the web client"
(cd "$REPO_DIR/crates/brumm-app" && trunk build --release)

echo "→ Syncing to $DEPLOY_HOST:$DEPLOY_DIR"
ssh "$DEPLOY_HOST" "mkdir -p '$DEPLOY_DIR/web-dist'"
rsync -az "$DEPLOY_DIR_LOCAL/Caddyfile" "$DEPLOY_DIR_LOCAL/docker-compose.yml" "$ENV_FILE" \
  "$DEPLOY_HOST:$DEPLOY_DIR/"
# --delete: old hashed bundles go away; index.html is the only entry point.
rsync -az --delete --exclude '.stage' --exclude '.trunk-lock' "$DIST_DIR/" "$DEPLOY_HOST:$DEPLOY_DIR/web-dist/"

echo "→ Starting the static server"
ssh "$DEPLOY_HOST" "cd '$DEPLOY_DIR' && sudo docker compose up -d --force-recreate"

echo "✓ Deployed${PUBLIC_URL:+ to $PUBLIC_URL}"
