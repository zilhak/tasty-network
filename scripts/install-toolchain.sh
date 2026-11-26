#!/usr/bin/env bash
# rust-toolchain.toml의 채널을 rustfmt·clippy와 함께 설치한다. 이미 있으면 rustup이 그대로 둔다.
# CI 잡이 쓴다. 사용: bash scripts/install-toolchain.sh
set -euo pipefail

cd "$(dirname "$0")/.."

channel=$(sed -n 's/^channel *= *"\([^"]*\)".*/\1/p' rust-toolchain.toml)
[ -n "$channel" ] || { echo "[install-toolchain] rust-toolchain.toml에서 channel을 읽지 못했다." >&2; exit 1; }
rustup toolchain install "$channel" --profile minimal -c rustfmt -c clippy
rustup show active-toolchain
cargo --version
