#!/usr/bin/env bash
# 이 clone의 Git 훅 경로를 .githooks로 정하고 ShellCheck와 Cargo 의존을 준비한다.
# 사용: bash scripts/dev-setup.sh. ShellCheck 설치와 의존 받기의 실패는 안내만 하고 계속한다.
# core.hooksPath는 clone의 공용 설정이라 그 clone의 모든 worktree가 함께 쓴다.

set -e

REPO_ROOT="$(git rev-parse --show-toplevel 2>/dev/null)" || {
    echo "✘ git 저장소 안에서 실행해 주세요." >&2
    exit 1
}

cd "$REPO_ROOT"

red()    { printf '\033[31m%s\033[0m\n' "$*" >&2; }
green()  { printf '\033[32m%s\033[0m\n' "$*"; }
yellow() { printf '\033[33m%s\033[0m\n' "$*"; }

yellow "▸ tasty-network 개발 환경 준비"
echo

yellow "[1/3] git 훅 설정 (core.hooksPath = .githooks)"

if [ ! -d .githooks ]; then
    red "  ✘ .githooks 폴더가 없습니다. checkout이 온전한지 확인해 주세요."
    exit 1
fi

CURRENT="$(git config --local --get core.hooksPath || echo '')"
if [ "$CURRENT" = ".githooks" ]; then
    green "  이미 설정됨"
else
    git config --local core.hooksPath .githooks
    green "  설정 완료"
fi

chmod +x .githooks/* 2>/dev/null || true

echo
yellow "[2/3] 셸 정적 검사기 (shellcheck)"
if bash scripts/install-shellcheck.sh; then
    green "  준비됨"
else
    yellow "  ▸ 받지 못했습니다. 배포판 패키지(apt·brew·pacman의 shellcheck)로 받아도 됩니다."
    yellow "    그때까지 셸 파일을 커밋하면 pre-commit의 A.3이 판정 불가로 커밋을 막습니다."
fi

echo
yellow "[3/3] Cargo 의존 (cargo fetch --locked)"
if ! command -v cargo >/dev/null 2>&1; then
    yellow "  ▸ cargo가 없습니다. Rust를 설치한 뒤 cargo fetch --locked를 한 번 실행해 주세요."
elif cargo fetch --locked; then
    green "  준비됨"
else
    yellow "  ▸ 받지 못했습니다. 네트워크를 확인하고 cargo fetch --locked를 다시 실행해 주세요."
fi

echo
green "✓ 준비 완료"
echo "이제 git commit·git merge·git push 때 검사가 돕니다. 검사 범위: docs/dev-guide/git-hooks.md"
