#!/usr/bin/env bash
#
# Downloads the Whisper model used for wake-word speech-to-text.
#
# Only needs curl (or wget) and writes to ../data/model.bin. No system-level
# installs. Pass a model name (see WHISPER_MODELS) as the first argument.
#
#   scripts/setup.sh            # download the default model
#   scripts/setup.sh small.en   # download a different model
#
# For quick "does it work" tests, base.en-q5_1 (the default) is a good
# balance of accuracy, speed, and size. Cut to tiny.en for the absolute
# lowest CPU usage; bump to small.en for better accuracy on song titles.
set -euo pipefail

WHISPER_MODELS="tiny tiny-q5_1 tiny.en tiny.en-q5_1 base base-q5_1 base.en base.en-q5_1 small small-q5_1 small.en small.en-q5_1"

DEFAULT_MODEL="base.en-q5_1"
MODEL="${1:-$DEFAULT_MODEL}"

BASE_URL="https://huggingface.co/ggerganov/whisper.cpp/resolve/main"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
DATA_DIR="$SCRIPT_DIR/../data"
OUTPUT="$DATA_DIR/model.bin"

if ! echo "$WHISPER_MODELS" | grep -qw "$MODEL"; then
    echo "Unknown model '$MODEL'. Pick one of: $WHISPER_MODELS" >&2
    exit 1
fi

mkdir -p "$DATA_DIR"

if command -v curl >/dev/null 2>&1; then
    curl -L --fail --progress-bar -o "$OUTPUT" "$BASE_URL/ggml-$MODEL.bin"
elif command -v wget >/dev/null 2>&1; then
    wget --show-progress --output-document "$OUTPUT" "$BASE_URL/ggml-$MODEL.bin"
else
    echo "Neither curl nor wget is installed; cannot download the model." >&2
    exit 1
fi

echo "Downloaded ggml-$MODEL.bin -> $OUTPUT"

for tool in yt-dlp ffmpeg; do
    if ! command -v "$tool" >/dev/null 2>&1; then
        echo "WARNING: '$tool' is missing; music playback will not work." >&2
    fi
done