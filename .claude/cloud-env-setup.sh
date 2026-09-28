#!/bin/bash
# Setup script for the Claude Code cloud environment (claude.ai/code).
#
# Paste this file's contents into the environment's "Setup script" field.
# It runs once as root on Ubuntu 24.04 before Claude starts, and the result
# is snapshotted and reused, so it holds the slow VM-level installs: system
# packages and the Rust toolchain. Per-session project setup (cargo fetch,
# eslint) lives in .claude/session-start.sh instead.
#
# Must exit 0 and finish in about 5 minutes or the snapshot is not cached.

export DEBIAN_FRONTEND=noninteractive

apt-get update

# zmng runtime: detection, thumbnails and the zmNinjaNg MJPEG path shell out
# to ffmpeg. The lavfi:testsrc sub-url lets the detector run without a camera.
apt-get install -y --no-install-recommends ffmpeg sqlite3 || true

# Legacy C++ daemons + Catch2 suite (same list as .github/workflows/ci-cpp-tests.yml).
apt-get install -y --no-install-recommends \
  build-essential cmake pkg-config \
  catch2 \
  libavcodec-dev libavformat-dev libavutil-dev libavdevice-dev \
  libswresample-dev libswscale-dev \
  default-libmysqlclient-dev \
  libbz2-dev libcurl4-openssl-dev libjpeg-dev libturbojpeg0-dev \
  libpcre2-dev libpolkit-gobject-1-dev libssl-dev zlib1g-dev \
  libv4l-dev libvlc-dev libvncserver-dev \
  libmosquitto-dev libmosquittopp-dev \
  gsoap libgsoap-dev || true

# The image ships a Rust toolchain; make sure it is current stable with the
# components used for linting.
if command -v rustup >/dev/null 2>&1; then
  rustup update stable || true
  rustup default stable || true
  rustup component add clippy rustfmt || true
else
  curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal -c clippy,rustfmt || true
fi

exit 0
