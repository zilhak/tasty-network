#!/usr/bin/env bash
# 이 레포의 검사 진입점. git 훅, CI, 원격 빌드가 모두 이 스크립트의 단계를 부른다.
#
#   scripts/check.sh [all]          fmt + shell + no-tasty-deps + p2p-isolation + no-hidden-constants
#                                   + no-process-spawn + lint + test + transport-no-ssh
#   scripts/check.sh fmt            cargo fmt --all --check
#   scripts/check.sh lint           cargo clippy (workspace, 모든 target, -D warnings)
#   scripts/check.sh test [ARGS]    cargo test --workspace. ARGS는 cargo test에 그대로 넘긴다
#   scripts/check.sh check          cargo check --workspace --all-targets
#   scripts/check.sh transport-no-ssh
#                                   그래프 판정 자체 시험 뒤 tasty-net-transport를 SSH 없이(--no-default-features)
#                                   clippy·시험·문서(깨진 문서 링크는 실패)하고, 그 의존 그래프에
#                                   tasty-net-ssh가 없는지 본다
#   scripts/check.sh shell          추적된 셸 자산 전체의 shellcheck (scripts/check-shell-assets.sh)
#   scripts/check.sh no-tasty-deps  판정 자체 시험 뒤 tasty 레포 크레이트 의존 없음 (scripts/check-no-tasty-deps.sh)
#   scripts/check.sh p2p-isolation  판정 자체 시험 뒤 P2P 크레이트의 workspace 안 의존 범위 (scripts/check-p2p-isolation.sh)
#   scripts/check.sh no-hidden-constants
#                                   판정 자체 시험 뒤 숨은 동작값 없음과 설정 표 키 대조
#                                   (scripts/check-no-hidden-constants.sh, 목록 scripts/no-hidden-constants.list)
#   scripts/check.sh no-process-spawn
#                                   판정 자체 시험 뒤 모바일 대상 라이브러리 코드의 다른 프로그램 실행 없음
#                                   (scripts/check-no-process-spawn.sh)
#   scripts/check.sh p2p-example-pair
#                                   자체 시험 뒤 서버와 p2p_peer 예제 두 개를 루프백에 띄워 펀칭·연결·echo
#                                   (scripts/check-p2p-example-pair.sh). all에 없고 CI의 clippy·test 잡이 부른다
#   scripts/check.sh deny           cargo deny의 licenses·bans·sources (deny.toml). cargo-deny가 필요하고 all에 없다
#   scripts/check.sh mobile-check <target>…
#                                   의존 쪽 검사 자체 시험 뒤 target마다 scripts/mobile-crates.list의 조합을
#                                   cargo check --lib하고 의존 쪽 검사(scripts/check-mobile-deps.sh)를 돌린다.
#                                   target·NDK·Xcode가 없으면 건너뛰고 한 줄로 알린다(MOBILE_CHECK_REQUIRE=1이면
#                                   실패). Android는 ANDROID_NDK_ROOT(또는 ANDROID_NDK_HOME)의 NDK를 쓴다. all에 없다
#
# 의존을 해석하는 cargo 명령(clippy·test·check·tree·doc·metadata·deny)은 --locked라 Cargo.lock을 바꾸지 않는다.
# cargo fmt는 의존을 해석하지 않아 --locked가 없다.
# Linux·macOS의 bash와 Windows의 Git Bash에서 돈다.
# fmt·lint·test·check·all은 이 clone에서 .githooks가 돌지 않으면 stderr에 경고한다.
set -euo pipefail
# cd가 CDPATH로 폴더를 찾으면 다른 폴더로 갈 수 있고 그 경로를 stdout에 찍는다($(cd … && pwd)의 값은
# 두 줄이 된다). 이 스크립트의 cd는 모두 주어진 경로 그대로 간다.
unset CDPATH

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
# transport의 의존 그래프(모든 target, 일반·빌드·dev 의존)를 패키지마다 한 줄로 찍는다.
# 자체 시험이 이 함수를 가짜로 바꿔 끼운다.
transport_tree() {
    # 출력을 읽으므로 색은 늘 끈다(CARGO_TERM_COLOR=always여도 평문).
    run cargo tree --locked --color never -p tasty-net-transport "$@" --target all -e normal,build,dev \
        --prefix none -f '{p}'
}
# 그 그래프에 tasty-net-ssh가 있으면 yes, 없으면 no를 찍는다. cargo tree가 실패하거나 출력에
# tasty-net-transport 줄이 없으면 판정하지 않고 1로 끝난다. 명령 치환 안에서는 errexit가 꺼지므로
# 종료 코드를 직접 본다.
transport_tree_has_ssh() {
    local tree rc=0
    tree=$(transport_tree "$@") || rc=$?
    if [[ $rc -ne 0 ]]; then
        echo "cargo tree가 실패했다(종료 코드 $rc, 인자: ${*:-기본 feature})" >&2
        return 1
    fi
    if ! grep -q '^tasty-net-transport v' <<<"$tree"; then
        echo "cargo tree 출력에 tasty-net-transport가 없다" >&2
        return 1
    fi
    if grep -q '^tasty-net-ssh v' <<<"$tree"; then echo yes; else echo no; fi
}
# 그래프 판정: 기본 feature의 그래프에 tasty-net-ssh가 보이는지 먼저 확인해 판정이 그 줄을 찾을 수
# 있는지 대조하고, --no-default-features 그래프에 그 줄이 없어야 통과(0)다.
transport_graph_verdict() {
    local got
    if ! got=$(transport_tree_has_ssh); then
        return 1
    fi
    if [[ "$got" != yes ]]; then
        echo "대조 실패: 기본 feature의 transport 그래프에서 tasty-net-ssh를 찾지 못했다" >&2
        return 1
    fi
    if ! got=$(transport_tree_has_ssh --no-default-features); then
        return 1
    fi
    if [[ "$got" != no ]]; then
        echo "실패: --no-default-features의 transport 그래프에 tasty-net-ssh가 있다" >&2
        return 1
    fi
}
# 그래프 판정의 자체 시험: 가짜 cargo tree(출력과 종료 코드)를 넣어 통과·실패를 본다.
transport_no_ssh_self_test() {
    local root='tasty-net-transport v0.1.0 (/w/crates/tasty-net-transport)'
    local ssh='tasty-net-ssh v0.1.0 (/w/crates/tasty-net-ssh)'
    local other='tracing v0.1.44'
    local failed=0 name want on_out on_rc off_out off_rc got
    # 이름|기대(0 통과, 1 실패)|기본 출력|기본 종료 코드|SSH 없음 출력|SSH 없음 종료 코드
    local cases=(
        "정상: 기본에 있고 SSH 없음에 없다|0|$root;$ssh;$other|0|$root;$other|0"
        "SSH 없음 그래프에 tasty-net-ssh가 남았다|1|$root;$ssh|0|$root;$ssh|0"
        "기본 그래프에 tasty-net-ssh가 없다(대조 실패)|1|$root;$other|0|$root;$other|0"
        "SSH 없음 cargo tree가 루트 줄을 찍고 42로 끝난다|1|$root;$ssh|0|$root|42"
        "기본 cargo tree가 줄을 다 찍고 101로 끝난다|1|$root;$ssh|101|$root|0"
        "SSH 없음 출력에 루트 줄이 없다|1|$root;$ssh|0||0"
    )
    for c in "${cases[@]}"; do
        IFS='|' read -r name want on_out on_rc off_out off_rc <<<"$c"
        got=0
        (
            transport_tree() {
                local out=$on_out rc=$on_rc
                if [[ " $* " == *" --no-default-features "* ]]; then
                    out=$off_out
                    rc=$off_rc
                fi
                [[ -n "$out" ]] && tr ';' '\n' <<<"$out"
                return "$rc"
            }
            transport_graph_verdict
        ) >/dev/null 2>&1 || got=1
        if [[ "$got" == "$want" ]]; then
            echo "자체 시험 통과: $name" >&2
        else
            echo "자체 시험 실패: $name (기대 $want, 결과 $got)" >&2
            failed=1
        fi
    done
    return "$failed"
}
# SSH feature 계약 1단계(docs/design/ssh-backends.md "feature 계약"): 그래프 판정의 자체 시험 뒤,
# SSH 없는 transport의 그래프에 tasty-net-ssh가 없고(기본 feature 그래프와 대조) 그 조합이 선다.
cmd_transport_no_ssh() {
    transport_no_ssh_self_test
    transport_graph_verdict
    echo "transport --no-default-features 그래프에 tasty-net-ssh 없음(대조: 기본 feature에는 있음)" >&2
    run cargo clippy --locked -p tasty-net-transport --no-default-features --all-targets -- -D warnings
    run cargo test --locked -p tasty-net-transport --no-default-features
    # SSH 없는 문서도 남는 항목의 링크가 모두 풀린다(조건부 항목을 무조건 링크하지 않는다).
    RUSTDOCFLAGS="-D rustdoc::broken_intra_doc_links" \
        run cargo doc --locked -p tasty-net-transport --no-default-features --no-deps
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
cmd_no_process_spawn() {
    run bash scripts/check-no-process-spawn.sh --self-test
    run bash scripts/check-no-process-spawn.sh
}
# 모바일 cross check(docs/design/mobile.md "모바일 cross check"). target 목록과 Android API 수준은
# 그 문서의 "대상 target" 표다.
MOBILE_TARGETS=(aarch64-linux-android x86_64-linux-android aarch64-apple-ios aarch64-apple-ios-sim)
MOBILE_ANDROID_API=29
# target의 std가 지금 툴체인에 있으면 0.
mobile_target_installed() {
    local sysroot
    sysroot=$(rustc --print sysroot) || return 1
    [[ -d "$sysroot/lib/rustlib/$1/lib" ]]
}
# Android target의 C 컴파일러·아카이버·링커를 NDK로 맞춘다(설계 "대상 target"의 환경 변수 표).
mobile_android_env() {
    local t=$1 ndk=$2 host bin ext="" var
    case "$(uname -s)" in
        Linux) host=linux-x86_64 ;;
        Darwin) host=darwin-x86_64 ;;
        MINGW* | MSYS* | CYGWIN*) host=windows-x86_64 ext=.cmd ;;
        *) echo "mobile-check: NDK 호스트를 모르는 OS다: $(uname -s)" >&2; return 1 ;;
    esac
    bin="$ndk/toolchains/llvm/prebuilt/$host/bin"
    if [[ ! -e "$bin/$t$MOBILE_ANDROID_API-clang$ext" || ! -e "$bin/llvm-ar" && ! -e "$bin/llvm-ar.exe" ]]; then
        echo "mobile-check: NDK($ndk)에 $t$MOBILE_ANDROID_API-clang${ext}·llvm-ar가 없다" >&2
        return 1
    fi
    var=${t//-/_}
    local cc="$bin/$t$MOBILE_ANDROID_API-clang$ext" up
    up=$(tr '[:lower:]' '[:upper:]' <<<"$var")
    export "CC_$var=$cc" "AR_$var=$bin/llvm-ar" "CARGO_TARGET_${up}_LINKER=$cc"
    echo "mobile-check: NDK $ndk (Pkg.Revision $(sed -n 's/^Pkg\.Revision *= *//p' "$ndk/source.properties" 2>/dev/null | tr -d '\r'))," \
        "CC_$var=$cc AR_$var=$bin/llvm-ar CARGO_TARGET_${up}_LINKER=$cc" >&2
}
# 조합 목록 파일. 자체 시험이 가짜 목록으로 바꾼다.
MOBILE_LIST=scripts/mobile-crates.list
# 조합 목록의 조합을 한 줄씩 "크레이트 feature 인자…"로 찍는다.
mobile_combos() {
    grep -v -e '^[[:space:]]*#' -e '^[[:space:]]*$' "$MOBILE_LIST"
}
# target 하나의 조합 전부. 실패해도 남은 조합을 돌리고 끝에 실패를 알린다.
mobile_check_target() {
    local t=$1 line crate failed=0
    local -a feats
    while IFS= read -r -u 3 line; do
        read -r -a feats <<<"$line"
        crate=${feats[0]}
        feats=("${feats[@]:1}")
        run cargo check --locked --target "$t" -p "$crate" --lib ${feats[@]+"${feats[@]}"} || failed=1
        run bash scripts/check-mobile-deps.sh --target "$t" -p "$crate" -- ${feats[@]+"${feats[@]}"} || failed=1
    done 3< <(mobile_combos)
    return "$failed"
}
# iOS target의 SDK 이름. 기기 target은 iphoneos, 시뮬레이터 target은 iphonesimulator다.
mobile_ios_sdk() {
    case "$1" in
        aarch64-apple-ios) echo iphoneos ;;
        aarch64-apple-ios-sim) echo iphonesimulator ;;
        *) return 1 ;;
    esac
}
# target 하나. 돌렸으면 그 결과를 돌려주고, 건너뛰면 부른 쪽의 skipped에 이유를 넣고 0을 돌려준다.
mobile_check_one() {
    local t=$1 ndk sdk
    if ! mobile_target_installed "$t"; then
        skipped+=("$t(rustup target add $t)")
        return 0
    fi
    case "$t" in
        *-linux-android)
            ndk=${ANDROID_NDK_ROOT:-${ANDROID_NDK_HOME:-}}
            if [[ -z "$ndk" || ! -d "$ndk" ]]; then
                skipped+=("$t(NDK 없음: ANDROID_NDK_ROOT)")
                return 0
            fi
            ( mobile_android_env "$t" "$ndk" && mobile_check_target "$t" )
            ;;
        *-apple-ios*)
            sdk=$(mobile_ios_sdk "$t") || return 1
            if ! xcrun --sdk "$sdk" --show-sdk-path >/dev/null 2>&1; then
                skipped+=("$t(Xcode $sdk SDK 없음)")
                return 0
            fi
            echo "mobile-check: $t SDK $sdk $(xcrun --sdk "$sdk" --show-sdk-version 2>/dev/null)" \
                "($(xcrun --sdk "$sdk" --show-sdk-path 2>/dev/null)), $(xcrun clang --version 2>/dev/null | head -n 1)" >&2
            mobile_check_target "$t"
            ;;
        *) return 1 ;;
    esac
}
# mobile-check 자체의 시험: 가짜 xcrun·target·조합 실행으로 target마다 고른 SDK와 건너뛰기를 본다.
mobile_check_self_test() {
    local failed=0 c name sdks t want got
    # 이름#가짜 xcrun이 아는 SDK(,로 나눈다)#target#기대(checked·skipped)
    local cases=(
        "기기 SDK만 있고 기기 target#iphoneos#aarch64-apple-ios#checked"
        "기기 SDK만 있고 시뮬레이터 target#iphoneos#aarch64-apple-ios-sim#skipped"
        "시뮬레이터 SDK만 있고 시뮬레이터 target#iphonesimulator#aarch64-apple-ios-sim#checked"
        "시뮬레이터 SDK만 있고 기기 target#iphonesimulator#aarch64-apple-ios#skipped"
        "두 SDK가 있고 시뮬레이터 target#iphoneos,iphonesimulator#aarch64-apple-ios-sim#checked"
        "SDK가 없고 기기 target##aarch64-apple-ios#skipped"
    )
    for c in "${cases[@]}"; do
        IFS='#' read -r name sdks t want <<<"$c"
        got=$(
            # shellcheck disable=SC2329  # mobile_check_one이 부른다
            xcrun() {
                if [[ "$1" == --sdk ]]; then
                    [[ ",$sdks," == *",$2,"* ]] || return 1
                    [[ "$3" == --show-sdk-version ]] && echo 99.0 || echo "/sdk/$2"
                    return 0
                fi
                echo "fake clang"
            }
            # shellcheck disable=SC2329
            mobile_target_installed() { return 0; }
            ran=""
            # shellcheck disable=SC2329
            mobile_check_target() { ran=$1; }
            skipped=()
            mobile_check_one "$t" 2>/dev/null || echo "rc=$?"
            if [[ ${#skipped[@]} -gt 0 ]]; then echo skipped; elif [[ "$ran" == "$t" ]]; then echo checked; fi
        )
        if [[ "$got" == "$want" ]]; then
            echo "mobile-check: 자체 시험 통과: $name" >&2
        else
            echo "mobile-check: 자체 시험 실패: $name (기대 $want, 결과 ${got:-없음})" >&2
            failed=1
        fi
    done
    # 조합 목록: 사전 검사(조합이 있는가)와 target마다의 루프가 같은 조합을 보는지 본다. 마지막 줄에 LF가
    # 없는 목록을 넣는다.
    # 이름|목록 내용(printf 형식)|루프가 돌린 조합(;로 나눈다)
    local tmp list
    tmp=$(mktemp -d "${TMPDIR:-/tmp}/mobile-check-test.XXXXXX")
    cases=(
        "마지막 줄에 LF가 없는 목록|tasty-net-frame\\ntasty-net-p2p|tasty-net-frame;tasty-net-p2p"
        "주석·빈 줄·feature 인자가 있고 마지막 줄에 LF가 없는 목록|# c\\n\\ntasty-net-frame\\n  # x\\ntasty-net-transport --no-default-features|tasty-net-frame;tasty-net-transport --no-default-features"
        "주석만 있고 마지막 줄에 LF가 없는 목록|# a\\n# b|"
    )
    for c in "${cases[@]}"; do
        IFS='|' read -r name list want <<<"$c"
        # shellcheck disable=SC2059  # 목록 내용은 printf 형식이다
        printf "$list" >"$tmp/list"
        got=$(
            MOBILE_LIST="$tmp/list"
            seen=""
            # shellcheck disable=SC2329  # mobile_check_target이 부른다
            run() {
                local a prev="" p="" after=0 rest=""
                [[ "$1" == cargo ]] || return 0
                for a in "$@"; do
                    if [[ $after -eq 1 ]]; then rest+=" $a"; elif [[ "$prev" == -p ]]; then p=$a; fi
                    [[ "$a" == --lib ]] && after=1
                    prev=$a
                done
                seen+="${seen:+;}$p$rest"
            }
            pre=0
            [[ -n "$(mobile_combos)" ]] && pre=1
            mobile_check_target aarch64-linux-android >/dev/null 2>&1
            echo "$pre#$seen"
        )
        local want_pre=0
        [[ -n "$want" ]] && want_pre=1
        if [[ "$got" == "$want_pre#$want" ]]; then
            echo "mobile-check: 자체 시험 통과: $name" >&2
        else
            echo "mobile-check: 자체 시험 실패: $name (기대 사전 검사#조합 $want_pre#$want, 결과 $got)" >&2
            failed=1
        fi
    done
    rm -rf "${tmp:?}"
    return "$failed"
}
cmd_mobile_check() {
    local t k skipped=() failed=0 known
    if [[ $# -eq 0 ]]; then
        echo "사용: scripts/check.sh mobile-check <target>… (target: ${MOBILE_TARGETS[*]})" >&2
        return 2
    fi
    for t in "$@"; do
        known=0
        for k in "${MOBILE_TARGETS[@]}"; do [[ "$k" == "$t" ]] && known=1; done
        if [[ $known -eq 0 ]]; then
            echo "mobile-check: 모바일 target이 아니다: $t (target: ${MOBILE_TARGETS[*]})" >&2
            return 2
        fi
    done
    [[ -n "$(mobile_combos)" ]] || { echo "mobile-check: scripts/mobile-crates.list에 조합이 없다" >&2; return 2; }
    mobile_check_self_test || return 1
    run bash scripts/check-mobile-deps.sh --self-test
    for t in "$@"; do
        mobile_check_one "$t" || failed=1
    done
    if [[ ${#skipped[@]} -gt 0 ]]; then
        echo "mobile-check: target 없음 — 건너뛴 target: ${skipped[*]}" >&2
        if [[ "${MOBILE_CHECK_REQUIRE:-}" == 1 ]]; then
            echo "mobile-check: MOBILE_CHECK_REQUIRE=1이라 건너뛰기를 실패로 본다" >&2
            failed=1
        fi
    fi
    return "$failed"
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
        echo "경고: git 훅을 이 트리의 .githooks가 아니라 ${got}에서 읽는다. bash scripts/dev-setup.sh를 실행한다." >&2
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
        cmd_no_process_spawn
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
    no-process-spawn) cmd_no_process_spawn ;;
    p2p-example-pair) cmd_p2p_example_pair ;;
    deny) cmd_deny ;;
    mobile-check) cmd_mobile_check "$@" ;;
    -h | --help | help)
        sed -n '2,36p' "$0" | sed 's/^# \{0,1\}//'
        ;;
    *)
        echo "모르는 단계: $sub (scripts/check.sh help)" >&2
        exit 2
        ;;
esac
