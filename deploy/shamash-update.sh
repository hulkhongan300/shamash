#!/bin/sh
# Pulls the newest Shamash image and restarts the bot if it changed.
#
# Run by shamash-update.timer every few minutes. Safe to run by hand: when
# nothing new was published it does nothing, and the bot keeps playing.
set -eu

cd "${SHAMASH_DIR:?set SHAMASH_DIR to the folder with compose.yaml and .env}"

running() {
  id=$(docker compose ps -q shamash 2>/dev/null || true)
  if [ -n "$id" ]; then
    docker inspect --format '{{.Image}}' "$id"
  else
    echo none
  fi
}

before=$(running)
docker compose pull --quiet shamash
# Recreates the container only when the image or the settings changed.
docker compose up -d --remove-orphans
after=$(running)

if [ "$before" != "$after" ]; then
  echo "updated: $(echo "${before#sha256:}" | cut -c1-12) -> $(echo "${after#sha256:}" | cut -c1-12)"
  # The old image is about 1 GB; drop it once nothing uses it.
  docker image prune -f >/dev/null
else
  echo "already up to date"
fi
