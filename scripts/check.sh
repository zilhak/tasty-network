#!/usr/bin/env bash
# 이 레포의 검사 진입점. git 훅, CI, 원격 빌드가 모두 이 스크립트의 단계를 부른다.
#
#   scripts/check.sh [all]          fmt + shell + no-tasty-deps + p2p-isolation + no-hidden-constants
#                                   + lint + test + transport-no-ssh
#   scripts/check.sh fmt            cargo fmt --all --check
#   scripts/check.sh lint           cargo clippy (workspace, 모든 target, -D warnings)
#   scripts/check.sh test [ARGS]    cargo test --workspace. ARGS는 cargo test에 그대로 넘긴다
#   scripts/check.sh check          cargo check --workspace --all-targets
#   scripts/check.sh transport-no-ssh
#                                   tasty-net-transport를 SSH 없이(--no-default-features) clippy·시험하고,
#                                   그 의존 그래프에 tasty-net-ssh가 없는지 본다
#   scripts/check.sh shell          추적된 셸 자산 전체의 shellcheck (scripts/check-shell-assets.sh)
#   scripts/check.sh no-tasty-deps  판정 자체 시험 뒤 tasty 레포 크레이트 의존 없음 (scripts/check-no-tasty-deps.sh)
#   scripts/check.sh p2p-isolation  판정 자체 시험 뒤 P2P 크레이트의 workspace 안 의존 범위 (scripts/check-p2p-isolation.sh)
#   scripts/check.sh no-hidden-constants
#                                   판정 자체 시험 뒤 숨은 동작값 없음과 설정 표 키 대조
#                                   (scripts/check-no-hidden-constants.sh, 목록 scripts/no-hidden-constants.list)
#   scripts/check.sh p2p-example-pair
#                                   자체 시험 뒤 서버와 p2p_peer 예제 두 개를 루프백에 띄워 펀칭·연결·echo
#                                   (scripts/check-p2p-example-pair.sh). all에 없고 CI의 clippy·test 잡이 부른다
#   scripts/check.sh deny           cargo deny의 licenses·bans·sources (deny.toml). cargo-deny가 필요하고 all에 없다
#
# 의존을 해석하는 cargo 명령(clippy·test·check·tree·metadata·deny)은 --locked라 Cargo.lock을 바꾸지 않는다.
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
# transport의 의존 그래프(모든 target, 일반·빌드·dev 의존)에 tasty-net-ssh가 있으면 yes, 없으면 no.
transport_tree_has_ssh() {
    local tree
    tree=$(run cargo tree --locked -p tasty-net-transport "$@" --target all -e normal,build,dev \
        --prefix none -f '{p}')
    if ! grep -q '^tasty-net-transport v' <<<"$tree"; then
        echo "cargo tree 출력에 tasty-net-transport가 없다" >&2
        return 1
    fi
    if grep -q '^tasty-net-ssh v' <<<"$tree"; then echo yes; else echo no; fi
}
# SSH feature 계약 1단계(docs/design/ssh-backends.md "feature 계약"): SSH 없는 transport가 서고,
# 그 그래프에 tasty-net-ssh가 없다. 기본 feature의 그래프에 tasty-net-ssh가 보이는지 먼저 확인해
# 판정이 그 줄을 찾을 수 있는지 대조한다.
cmd_transport_no_ssh() {
    local got
    got=$(transport_tree_has_ssh)
    if [[ "$got" != yes ]]; then
        echo "대조 실패: 기본 feature의 transport 그래프에서 tasty-net-ssh를 찾지 못했다" >&2
        return 1
    fi
    got=$(transport_tree_has_ssh --no-default-features)
    if [[ "$got" != no ]]; then
        echo "실패: --no-default-features의 transport 그래프에 tasty-net-ssh가 있다" >&2
        return 1
    fi
    echo "transport --no-default-features 그래프에 tasty-net-ssh 없음(대조: 기본 feature에는 있음)" >&2
    run cargo clippy --locked -p tasty-net-transport --no-default-features --all-targets -- -D warnings
    run cargo test --locked -p tasty-net-transport --no-default-features
}
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
cmd_p2p_example_pair() {
    run bash scripts/check-p2p-example-pair.sh --self-test
    run bash scripts/check-p2p-example-pair.sh
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
        cmd_transport_no_ssh
        ;;
    fmt) cmd_fmt ;;
    lint) cmd_lint ;;
    test) cmd_test "$@" ;;
    check) cmd_check ;;
    transport-no-ssh) cmd_transport_no_ssh ;;
    shell) cmd_shell ;;
    no-tasty-deps) cmd_no_tasty_deps ;;
    p2p-isolation) cmd_p2p_isolation ;;
    no-hidden-constants) cmd_no_hidden_constants ;;
    p2p-example-pair) cmd_p2p_example_pair ;;
    deny) cmd_deny ;;
    -h | --help | help)
        sed -n '2,27p' "$0" | sed 's/^# \{0,1\}//'
        ;;
    *)
        echo "모르는 단계: $sub (scripts/check.sh help)" >&2
        exit 2
        ;;
esac
