#!/usr/bin/env bash
# rust-toolchain.toml의 채널을 rustfmt·clippy와 함께 설치한다. 이미 있으면 rustup이 그대로 둔다.
# CI 잡이 쓴다. 사용: bash scripts/install-toolchain.sh
set -euo pipefail
# cd가 CDPATH로 폴더를 찾으면 다른 폴더로 갈 수 있고 그 경로를 stdout에 찍는다($(cd … && pwd)의 값은
# 두 줄이 된다). 이 스크립트의 cd는 모두 주어진 경로 그대로 간다.
unset CDPATH

cd "$(dirname "$0")/.."

channel=$(sed -n 's/^channel *= *"\([^"]*\)".*/\1/p' rust-toolchain.toml)
[ -n "$channel" ] || { echo "[install-toolchain] rust-toolchain.toml에서 channel을 읽지 못했다." >&2; exit 1; }
rustup toolchain install "$channel" --profile minimal -c rustfmt -c clippy
rustup show active-toolchain
cargo --version
