#!/usr/bin/env bash
# 바인딩 생성 검사와 rlib 링크 시험(docs/design/mobile.md "경계", "rlib 링크 계약").
# 시험용 cdylib(crates/tasty-net-ffi/link-test)은 자기 UniFFI export 하나와 tasty-net-ffi를 rlib으로 함께
# 싣는다. 그 라이브러리 하나를 입력으로 고정판 생성기를 라이브러리 모드로 돌려 Kotlin·Swift를 임시 폴더에
# 만들고 아래를 본다. 생성물은 버리고 커밋하지 않는다.
#   - 두 네임스페이스(tasty_net, tasty_link_test)의 Kotlin 파일과 Swift 파일이 모두 생긴다
#   - tasty_net의 Kotlin 패키지는 crates/tasty-net-ffi/uniffi.toml의 값(uniffi/tasty_net 폴더)이다
#   - 두 Kotlin 파일이 모두 입력 라이브러리 이름(tasty_net_ffi_link_test)을 싣는다(cdylib_name 없음)
#   - 라이브러리의 동적 심벌 표에 두 크레이트의 export가 정의되어 있다. nm으로 정의된 동적 심벌만 읽고
#     (Linux nm -D --defined-only, macOS nm -gU), 이름이 접두(uniffi_tasty_net_ffi_fn_,
#     uniffi_tasty_net_ffi_link_test_fn_)와 소문자·숫자·밑줄로만 된 함수 심벌(T)을 센다. 이름 일부만 맞는 줄과
#     미정의 참조(U)는 세지 않는다. 두 크레이트에서 정해 둔 심벌 하나씩(TastyNet 생성자, 시험 cdylib의
#     함수)이 꼭 있어야 한다. nm이 없거나 이 OS의 심벌 표를 읽을 줄 모르면 실패한다(건너뛰지 않는다)
#
# 사용: bash scripts/check-ffi-bindgen.sh               빌드·생성·판정
#       bash scripts/check-ffi-bindgen.sh --self-test   판정 함수를 가짜 생성 폴더로 시험한다
# 종료 코드: 통과 0, 실패 1(nm 없음 포함), 판정 불가(python3 없음) 2.

set -euo pipefail
# cd가 CDPATH로 폴더를 찾으면 다른 폴더로 갈 수 있고 그 경로를 stdout에 찍는다. 이 스크립트의 cd는 주어진
# 경로 그대로 간다.
unset CDPATH

cd "$(dirname "$0")/.."

TAG="[ffi-bindgen]"
LIB_NAME=tasty_net_ffi_link_test
NET_KT=uniffi/tasty_net/tasty_net.kt
APP_KT=uniffi/tasty_link_test/tasty_link_test.kt

run() {
    echo "+ $*" >&2
    "$@"
}

# 생성 폴더 $1 판정. 통과 0, 실패 1. 실패마다 한 줄을 stderr에 찍는다.
verdict() {
    local out=$1 failed=0 f
    for f in "$NET_KT" "$APP_KT" tasty_net.swift tasty_link_test.swift; do
        if [[ ! -s "$out/$f" ]]; then
            echo "${TAG} 실패: 생성물 없음: ${f}" >&2
            failed=1
        fi
    done
    for f in "$NET_KT" "$APP_KT"; do
        [[ -s "$out/$f" ]] || continue
        if ! grep -q "\"${LIB_NAME}\"" "$out/$f"; then
            echo "${TAG} 실패: ${f}가 입력 라이브러리 이름 ${LIB_NAME}을 싣지 않는다" >&2
            failed=1
        fi
    done
    if [[ -s "$out/$NET_KT" ]] && ! grep -q '^package uniffi\.tasty_net$' "$out/$NET_KT"; then
        echo "${TAG} 실패: ${NET_KT}의 package 줄이 uniffi.tasty_net이 아니다" >&2
        failed=1
    fi
    return "$failed"
}

NET_PREFIX=uniffi_tasty_net_ffi_fn_
APP_PREFIX=uniffi_tasty_net_ffi_link_test_fn_
# 두 크레이트에 꼭 있어야 하는 export 하나씩.
NET_REQUIRED=${NET_PREFIX}constructor_tastynet_new
APP_REQUIRED=${APP_PREFIX}func_link_test_device_id_len

# 정의된 동적 심벌 목록(nm 출력) 파일 $1에서 접두 $2의 함수 export 이름을 한 줄씩 찍는다. $3은 심벌 이름
# 앞에 붙는 글자(Mach-O는 _, ELF는 빈 값)다. 줄은 "주소 형 이름" 세 칸이어야 하고 형은 T(정의된 전역 함수)다.
exports_of() {
    local file=$1 prefix=$2 lead=$3
    awk -v want="^${lead}${prefix}[a-z0-9_]+\$" -v lead="$lead" '
        NF == 3 && $2 == "T" && $3 ~ want { print substr($3, length(lead) + 1) }
    ' "$file"
}

# nm 출력 파일 $1의 판정. 통과 0, 실패 1. 실패 줄과 센 수를 stderr에 찍는다.
symbols_verdict() {
    local file=$1 lead=$2 net app failed=0
    net=$(exports_of "$file" "$NET_PREFIX" "$lead")
    app=$(exports_of "$file" "$APP_PREFIX" "$lead")
    echo "${TAG} 정의된 export: tasty_net_ffi $(grep -c . <<<"$net" || true)개," \
        "tasty_net_ffi_link_test $(grep -c . <<<"$app" || true)개" >&2
    if ! grep -qx "$NET_REQUIRED" <<<"$net"; then
        echo "${TAG} 실패: 정의된 export에 ${NET_REQUIRED}가 없다" >&2
        failed=1
    fi
    if ! grep -qx "$APP_REQUIRED" <<<"$app"; then
        echo "${TAG} 실패: 정의된 export에 ${APP_REQUIRED}가 없다" >&2
        failed=1
    fi
    return "$failed"
}

# 라이브러리 $1의 정의된 동적 심벌 판정. 통과 0, 실패 1.
symbols() {
    local lib=$1 list lead rc=0
    if ! command -v nm >/dev/null 2>&1; then
        echo "${TAG} 실패: nm이 PATH에 없다(binutils가 필요하다)" >&2
        return 1
    fi
    list=$(mktemp "${TMPDIR:-/tmp}/ffi-bindgen-nm.XXXXXX")
    case "$(uname -s)" in
        Darwin) lead=_; nm -gU "$lib" >"$list" 2>/dev/null || rc=1 ;;
        Linux) lead=""; nm -D --defined-only "$lib" >"$list" 2>/dev/null || rc=1 ;;
        *)
            echo "${TAG} 실패: 이 OS($(uname -s))의 동적 심벌 표는 읽지 않는다" >&2
            rm -f "${list:?}"
            return 1
            ;;
    esac
    if [[ $rc -ne 0 ]]; then
        echo "${TAG} 실패: nm이 ${lib}를 읽지 못했다" >&2
    elif ! symbols_verdict "$list" "$lead"; then
        rc=1
    fi
    rm -f "${list:?}"
    return "$rc"
}

self_test() {
    local tmp failed=0 name want got
    tmp=$(mktemp -d "${TMPDIR:-/tmp}/ffi-bindgen-test.XXXXXX")
    # 이름|기대(0 통과, 1 실패)|만들 파일(;로 나눈다, 파일:내용)
    local good_net="package uniffi.tasty_net
return \"${LIB_NAME}\""
    local cases=(
        "모두 있다|0|net;app;swift;swift2"
        "앱 Kotlin이 없다|1|net;swift;swift2"
        "Swift가 없다|1|net;app"
        "다른 라이브러리 이름|1|netother;app;swift;swift2"
        "다른 패키지|1|netpkg;app;swift;swift2"
    )
    local c files part dir
    for c in "${cases[@]}"; do
        IFS='|' read -r name want files <<<"$c"
        dir="$tmp/case"
        rm -rf "${dir:?}"
        mkdir -p "$dir/uniffi/tasty_net" "$dir/uniffi/tasty_link_test"
        IFS=';' read -r -a parts <<<"$files"
        for part in "${parts[@]}"; do
            case "$part" in
                net) printf '%s\n' "$good_net" >"$dir/$NET_KT" ;;
                netother) printf 'package uniffi.tasty_net\nreturn "uniffi_tasty_net"\n' >"$dir/$NET_KT" ;;
                netpkg) printf 'package com.example\nreturn "%s"\n' "$LIB_NAME" >"$dir/$NET_KT" ;;
                app) printf 'package uniffi.tasty_link_test\nreturn "%s"\n' "$LIB_NAME" >"$dir/$APP_KT" ;;
                swift) echo "// swift" >"$dir/tasty_net.swift" ;;
                swift2) echo "// swift" >"$dir/tasty_link_test.swift" ;;
            esac
        done
        got=0
        verdict "$dir" 2>/dev/null || got=1
        if [[ "$got" == "$want" ]]; then
            echo "${TAG} 자체 시험 통과: ${name}" >&2
        else
            echo "${TAG} 자체 시험 실패: ${name} (기대 ${want}, 결과 ${got})" >&2
            failed=1
        fi
    done
    # 심벌 판정: 이름|기대|lead|nm 줄(;로 나눈다)
    local net_ok="0000000000001000 T ${NET_REQUIRED}"
    local app_ok="0000000000002000 T ${APP_REQUIRED}"
    local sym_cases=(
        "두 export가 정의되어 있다|0||${net_ok};${app_ok}"
        "Mach-O 이름 앞 밑줄|0|_|0000000000001000 T _${NET_REQUIRED};0000000000002000 T _${APP_REQUIRED}"
        "시험 cdylib export가 미정의 참조뿐|1||${net_ok};                 U ${APP_REQUIRED}"
        "이름 일부만 맞는다|1||${net_ok};0000000000002000 T x${APP_REQUIRED}"
        "뒤에 다른 글자가 붙는다|1||${net_ok};0000000000002000 T ${APP_REQUIRED}.cold"
        "지역 심벌(t)|1||${net_ok};0000000000002000 t ${APP_REQUIRED}"
        "정해 둔 심벌 없이 다른 export만|1||0000000000001000 T ${NET_PREFIX}func_other;${app_ok}"
    )
    local lead lines
    for c in "${sym_cases[@]}"; do
        IFS='|' read -r name want lead lines <<<"$c"
        tr ';' '\n' <<<"$lines" >"$tmp/nm.txt"
        got=0
        symbols_verdict "$tmp/nm.txt" "$lead" 2>/dev/null || got=1
        if [[ "$got" == "$want" ]]; then
            echo "${TAG} 자체 시험 통과: 심벌: ${name}" >&2
        else
            echo "${TAG} 자체 시험 실패: 심벌: ${name} (기대 ${want}, 결과 ${got})" >&2
            failed=1
        fi
    done
    rm -rf "${tmp:?}"
    return "$failed"
}

if [[ "${1:-}" == --self-test ]]; then
    self_test
    exit $?
fi
[[ $# -eq 0 ]] || { echo "${TAG} 모르는 인자: $*" >&2; exit 2; }

PY=""
for c in python3 python; do
    if command -v "$c" >/dev/null 2>&1; then PY="$c"; break; fi
done
[[ -n "$PY" ]] || { echo "${TAG} 판정 불가: python3가 PATH에 없다." >&2; exit 2; }

target_dir=$(cargo metadata --locked --format-version 1 --no-deps |
    "$PY" -c 'import json,sys; print(json.load(sys.stdin)["target_directory"])')
case "$(uname -s)" in
    Darwin) lib="$target_dir/debug/lib${LIB_NAME}.dylib"; exe="" ;;
    MINGW* | MSYS* | CYGWIN*) lib="$target_dir/debug/${LIB_NAME}.dll"; exe=.exe ;;
    *) lib="$target_dir/debug/lib${LIB_NAME}.so"; exe="" ;;
esac

run cargo build --locked -p tasty-net-ffi-link-test --lib --bin uniffi-bindgen
out=$(mktemp -d "${TMPDIR:-/tmp}/ffi-bindgen.XXXXXX")
rc=0
# --no-format: 생성기가 ktlint 같은 다른 프로그램을 부르지 않게 한다.
# 생성기는 라이브러리 모드에서 크레이트의 uniffi.toml을 찾으려고 안에서 cargo metadata를 --locked 없이 부른다.
# 바로 앞의 cargo build --locked가 의존 해석을 Cargo.lock 그대로 맞춰 두므로 그 호출이 Cargo.lock을 바꾸지 않는다.
run "$target_dir/debug/uniffi-bindgen$exe" generate --language kotlin --language swift \
    --no-format --out-dir "$out" "$lib" || rc=1
if [[ $rc -eq 0 ]]; then
    verdict "$out" || rc=1
fi
if ! symbols "$lib"; then
    echo "${TAG} 실패: 라이브러리의 동적 심벌 표에 두 크레이트의 export가 다 있지 않다" >&2
    rc=1
fi
if [[ $rc -eq 0 ]]; then
    echo "${TAG} 통과: ${LIB_NAME} 하나에서 tasty_net·tasty_link_test의 Kotlin·Swift가 생겼다" >&2
fi
rm -rf "${out:?}"
exit "$rc"
