#!/usr/bin/env bash
# 이 레포의 검사 진입점. git 훅, CI, 원격 빌드가 모두 이 스크립트의 단계를 부른다.
#
#   scripts/check.sh [all]          fmt + shell + no-tasty-deps + p2p-isolation + no-hidden-constants
#                                   + lint + test
#   scripts/check.sh fmt            cargo fmt --all --check
#   scripts/check.sh lint           cargo clippy (workspace, 모든 target, -D warnings)
#   scripts/check.sh test [ARGS]    cargo test --workspace. ARGS는 cargo test에 그대로 넘긴다
#   scripts/check.sh check          cargo check --workspace --all-targets
#   scripts/check.sh shell          추적된 셸 자산 전체의 shellcheck (scripts/check-shell-assets.sh)
#   scripts/check.sh no-tasty-deps  판정 자체 시험 뒤 tasty 레포 크레이트 의존 없음 (scripts/check-no-tasty-deps.sh)
#   scripts/check.sh p2p-isolation  판정 자체 시험 뒤 P2P 크레이트의 workspace 안 의존 범위 (scripts/check-p2p-isolation.sh)
#   scripts/check.sh no-hidden-constants
#                                   판정 자체 시험 뒤 숨은 동작값 없음과 설정 표 키 대조
#                                   (scripts/check-no-hidden-constants.sh, 목록 scripts/no-hidden-constants.list)
#   scripts/check.sh deny           cargo deny의 licenses·bans·sources (deny.toml). cargo-deny가 필요하고 all에 없다
#
# 의존을 해석하는 cargo 명령(clippy·test·check·metadata·deny)은 --locked라 Cargo.lock을 바꾸지 않는다.
# cargo fmt는 의존을 해석하지 않아 --locked가 없다.
# Linux·macOS의 bash와 Windows의 Git Bash에서 돈다.
# fmt·lint·test·check·all은 이 clone에서 .githooks가 돌지 않으면 stderr에 경고한다.
set -euo pipefail

cd "$(dirname "$0")/.."

run() {
    echo "+ $*" >&2
    "$@"
}

cmd_fmt() { run cargo fmt --all --check; }
cmd_lint() { run cargo clippy --workspace --all-targets --locked -- -D warnings; }
cmd_test() { run cargo test --workspace --locked "$@"; }
cmd_check() { run cargo check --workspace --all-targets --locked; }
cmd_shell() { run bash scripts/check-shell-assets.sh; }
cmd_no_tasty_deps() {
    run bash scripts/check-no-tasty-deps.sh --self-test
    run bash scripts/check-no-tasty-deps.sh
}
cmd_p2p_isolation() {
    run bash scripts/check-p2p-isolation.sh --self-test
    run bash scripts/check-p2p-isolation.sh
}
cmd_no_hidden_constants() {
    run bash scripts/check-no-hidden-constants.sh --self-test
    run bash scripts/check-no-hidden-constants.sh
}
# advisories는 커밋과 무관하게 새 공지로 실패하므로 여기서 보지 않는다(docs/dev-guide/ci.md).
cmd_deny() { run cargo deny --locked --workspace check licenses bans sources; }

# 이 트리의 .githooks가 여기서 돌지 않으면 stderr에 경고한다. 실패하지 않고 종료 코드도 바꾸지 않는다.
# CI checkout에는 훅이 없고 소스 묶음에는 git이 없으므로 둘 다 조용히 지나간다.
warn_hooks() {
    local top hp got want
    [[ "${GITHUB_ACTIONS:-}" == "true" ]] && return 0
    git rev-parse --is-inside-work-tree >/dev/null 2>&1 || return 0
    top=$(git rev-parse --show-toplevel 2>/dev/null) || return 0
    hp=$(git config --path --get core.hooksPath 2>/dev/null) || hp=""
    if [[ -z "$hp" ]]; then
        echo "경고: 이 clone에 core.hooksPath가 없어 .githooks의 커밋·merge·push 검사가 돌지 않는다." \
            "bash scripts/dev-setup.sh를 한 번 실행한다(clone의 모든 worktree가 이 설정을 함께 쓴다)." >&2
        return 0
    fi
    # Windows 드라이브 경로(C:/... 또는 C:\...)도 절대 경로다.
    if ! [[ "$hp" == /* || "$hp" =~ ^[A-Za-z]:[/\\] ]]; then
        hp="$top/$hp"
    fi
    if ! got=$(cd "$hp" 2>/dev/null && pwd -P); then
        echo "경고: core.hooksPath가 없는 폴더($hp)를 가리켜 git 훅이 돌지 않는다. bash scripts/dev-setup.sh를 실행한다." >&2
        return 0
    fi
    want=$(cd "$top/.githooks" 2>/dev/null && pwd -P) || want=""
    if [[ "$got" != "$want" ]]; then
        echo "경고: git 훅을 이 트리의 .githooks가 아니라 $got에서 읽는다. bash scripts/dev-setup.sh를 실행한다." >&2
    fi
    return 0
}

sub="${1:-all}"
[[ $# -gt 0 ]] && shift

case "$sub" in
    # || 목록 안에서는 함수 안의 errexit가 꺼지므로 경고 출력 실패가 실행을 멈추지 않는다.
    all | fmt | lint | test | check) ( warn_hooks ) || true ;;
esac

case "$sub" in
    all)
        cmd_fmt
        cmd_shell
        cmd_no_tasty_deps
        cmd_p2p_isolation
        cmd_no_hidden_constants
        cmd_lint
        cmd_test
        ;;
    fmt) cmd_fmt ;;
    lint) cmd_lint ;;
    test) cmd_test "$@" ;;
    check) cmd_check ;;
    shell) cmd_shell ;;
    no-tasty-deps) cmd_no_tasty_deps ;;
    p2p-isolation) cmd_p2p_isolation ;;
    no-hidden-constants) cmd_no_hidden_constants ;;
    deny) cmd_deny ;;
    -h | --help | help)
        sed -n '2,21p' "$0" | sed 's/^# \{0,1\}//'
        ;;
    *)
        echo "모르는 단계: $sub (scripts/check.sh help)" >&2
        exit 2
        ;;
esac
