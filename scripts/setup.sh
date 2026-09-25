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
# Every utterance is padded to whisper's 30 second context, so inference cost
# barely depends on how long the speech is: a two second "play lofi" costs the
# same as a long sentence. Measured on a 16 core CPU, per utterance:
#
#   base.en-q5_1 (default)   2.9 s    <- only one that feels instant
#   small.en-q5_1            6.6 s
#   large-v3-turbo-q5_0     27.6 s
#
# Bigger models transcribe marginally better on clean speech but are far slower,
# so the default stays at base.en-q5_1. Use small.en-q5_1 if accuracy matters
# more than the wait, and tiny.en for the lowest CPU usage.
set -euo pipefail

WHISPER_MODELS="tiny tiny-q5_1 tiny.en tiny.en-q5_1 base base-q5_1 base.en base.en-q5_1 small small-q5_1 small.en small.en-q5_1 large-v3-turbo large-v3-turbo-q5_0 large-v3-turbo-q8_0"

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