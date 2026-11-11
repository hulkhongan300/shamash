#!/bin/sh
# Starts the bot after two checks that save a confusing first hour.
set -eu

# yt-dlp stops working when YouTube changes, usually within weeks, and the
# image's copy only gets older. Updating at every start keeps playback working
# without rebuilding the image. YTDLP_UPDATE=off skips it.
case "${YTDLP_UPDATE:-on}" in
  off|false|0|no) ;;
  *) yt-dlp --update 2>&1 | tail -n 1 || echo "yt-dlp update failed; using $(yt-dlp --version)" ;;
esac

# Say up front whether a GPU was passed in, because without one a request
# takes ~17 s instead of ~1 s and nothing else would explain why.
if [ -e /dev/dri ] || [ -e /dev/nvidia0 ]; then
  echo "GPU device found; the startup log names the one each model bound to (backend=Vulkan0)."
else
  echo "No GPU passed into the container: speech recognition will run on the CPU, which is slow."
  echo "See the Docker section of the README to pass one in."
fi

exec "$@"
