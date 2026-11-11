# syntax=docker/dockerfile:1.7
#
# Shamash in a container, with GPU inference through Vulkan.
#
#   docker compose up -d                     # AMD / Intel GPU, or CPU
#   docker compose -f compose.yaml -f compose.nvidia.yaml up -d   # NVIDIA
#
# The image holds no models and no secrets. Models download into the /data
# volume on the first start; settings come from .env through compose.

ARG RUST_VERSION=1.98
ARG DEBIAN_RELEASE=trixie

# ---------------------------------------------------------------- build ----
# Built with cargo-chef, so the dependencies (transcribe.cpp, ggml and its
# Vulkan shaders: most of the build time) are compiled in a layer of their own
# that only changes with Cargo.lock. Layers are what CI's cache keeps; a
# `--mount=type=cache` folder is thrown away with the runner, which made every
# CI build compile everything from scratch.
FROM rust:${RUST_VERSION}-${DEBIAN_RELEASE} AS chef

# cmake and the C++ toolchain build transcribe.cpp's ggml; libvulkan-dev, glslc
# and spirv-headers are its Vulkan backend; libopus-dev is songbird's codec.
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      cmake pkg-config libopus-dev libvulkan-dev glslc spirv-headers \
 && rm -rf /var/lib/apt/lists/*

ARG CARGO_CHEF_VERSION=0.1.78
RUN cargo install cargo-chef --locked --version ${CARGO_CHEF_VERSION}
WORKDIR /src

# The recipe: the dependency graph, without the code. Unchanged code-only
# edits produce the same recipe, so the layer below stays cached.
FROM chef AS plan
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS build

# ggml tunes itself for the build machine's CPU (-march=native) unless
# SOURCE_DATE_EPOCH is set. An image is run on other machines, so it is: the
# binary then uses the portable instruction set and cannot crash with an
# illegal instruction on an older CPU. Inference is on the GPU, where this
# costs nothing. Build with `--build-arg PORTABLE=0` for a CPU-only box you
# build on yourself. Both steps below must see the same setting, or the second
# rebuilds ggml.
ARG PORTABLE=1

COPY --from=plan /src/recipe.json recipe.json
RUN if [ "$PORTABLE" = "1" ]; then export SOURCE_DATE_EPOCH=0; fi \
 && cargo chef cook --release --locked --recipe-path recipe.json

COPY . .
RUN if [ "$PORTABLE" = "1" ]; then export SOURCE_DATE_EPOCH=0; fi \
 && cargo build --release --locked \
 && cp target/release/shamash /usr/local/bin/shamash

# ---------------------------------------------------------------- tools ----
# Fetched in a stage of their own, so curl and unzip stay out of the image.
FROM debian:${DEBIAN_RELEASE}-slim AS tools
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates curl unzip \
 && rm -rf /var/lib/apt/lists/*

# yt-dlp solves YouTube's JavaScript challenges with deno, its default runtime.
# Debian's Node.js is too old for it (yt-dlp calls 20.x unsupported), and
# without a runtime some formats go missing.
ARG DENO_VERSION=2.9.8
RUN curl -fsSL -o /tmp/deno.zip \
      "https://github.com/denoland/deno/releases/download/v${DENO_VERSION}/deno-x86_64-unknown-linux-gnu.zip" \
 && unzip -q /tmp/deno.zip -d /usr/local/bin \
 && chmod 0755 /usr/local/bin/deno

# The standalone yt-dlp build bundles its Python and the challenge solver. It
# is pinned so the image builds the same way twice, and updated at every start
# by the entrypoint, because YouTube breaks old versions within weeks.
ARG YTDLP_VERSION=2026.10.26
RUN curl -fsSL -o /usr/local/bin/yt-dlp \
      "https://github.com/yt-dlp/yt-dlp/releases/download/${YTDLP_VERSION}/yt-dlp_linux" \
 && chmod 0755 /usr/local/bin/yt-dlp

# -------------------------------------------------------------- runtime ----
FROM debian:${DEBIAN_RELEASE}-slim

# ffmpeg transcodes, libopus0 is the voice codec, libvulkan1 is the Vulkan
# loader and mesa-vulkan-drivers the AMD (RADV) and Intel (ANV) drivers. NVIDIA
# brings its own driver in through the NVIDIA container toolkit.
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      ca-certificates ffmpeg libopus0 libvulkan1 mesa-vulkan-drivers \
 && rm -rf /var/lib/apt/lists/* \
 # Lavapipe is Vulkan on the CPU. With no GPU passed in it would be picked as
 # "the GPU" and be slower than ggml's own CPU backend, so it is removed.
 && rm -f /usr/share/vulkan/icd.d/lvp_icd*.json

COPY --from=tools /usr/local/bin/deno /usr/local/bin/yt-dlp /usr/local/bin/

COPY --from=build /usr/local/bin/shamash /usr/local/bin/shamash
COPY --chmod=0755 docker/entrypoint.sh /usr/local/bin/entrypoint.sh

# Everything that must survive a restart lives under /data: the models (about
# 1 GB, downloaded once), the song cache, and the bot's own data.
ENV HF_HOME=/data/models \
    XDG_CACHE_HOME=/data/cache \
    LIBRARY_PATH=/data/library.json
WORKDIR /data
VOLUME ["/data"]

ENTRYPOINT ["/usr/local/bin/entrypoint.sh"]
CMD ["shamash"]
