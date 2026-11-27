#!/usr/bin/env bash
# 고정 판의 Android NDK(Linux x86_64)를 받아 PREFIX 아래에 푼다. 이미 같은 판이 있으면 받지 않는다.
# 마지막 줄(stdout)에 NDK 폴더를 찍는다. 진행 줄은 stderr로 간다. CI의 mobile-android 잡이 쓴다.
#
# 사용: PREFIX=<폴더> bash scripts/install-ndk.sh      (기본 PREFIX: $HOME/.local/share/android-ndk)
#       ndk=$(PREFIX=<폴더> bash scripts/install-ndk.sh | tail -n 1)
#       bash scripts/install-ndk.sh --self-test          (가짜 아카이브로 설치 판정만 본다)
#
# 폴더를 찍는 것은 그 폴더의 source.properties가 고정 판이고 그 안에 clang이 있을 때뿐이다.
# PREFIX/android-ndk-<판>이 이미 있는데 그 판이 아니거나(source.properties 없음 포함) clang이 없으면
# 그 폴더를 건드리지 않고 2로 끝난다.
#
# 판과 대조값의 근거는 docs/dev-guide/ci.md "모바일 cross check"에 있다.
set -euo pipefail

NDK_RELEASE="r30"
NDK_REVISION="30.0.16248370"
# Google SDK 저장소 목록(https://dl.google.com/android/repository/repository2-3.xml)의
# ndk;30.0.16248370 linux 아카이브 checksum(SHA-1).
NDK_SHA1="5107f898313790e449e87eee2183d9a20602dee9"
# 같은 아카이브를 받아 잰 SHA-256(2026-11-28). SHA-1만으로 대조하지 않으려고 함께 고정한다.
NDK_SHA256="753611f410d002cfcd3f3dc2ef49aad532089d3180b436c060a90bf0fcb64df2"

NAME="android-ndk-$NDK_RELEASE"
ZIP="$NAME-linux.zip"
URL="https://dl.google.com/android/repository/$ZIP"
# NDK 폴더 안 clang의 자리. check.sh의 mobile-check가 같은 자리를 쓴다.
CLANG_REL="toolchains/llvm/prebuilt/linux-x86_64/bin/clang"

say() { echo "[install-ndk] $*" >&2; }

revision_of() {
    sed -n 's/^Pkg\.Revision *= *//p' "$1/source.properties" 2>/dev/null | tr -d '\r'
}

# 자체 시험이 아래 두 함수를 바꿔 끼운다.
host_ok() { [ "$(uname -s)" = Linux ] && [ "$(uname -m)" = x86_64 ]; }
fetch() { curl -fsSL --retry 3 -o "$1" "$URL"; }

sum256() {
    if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1" | cut -d' ' -f1; else shasum -a 256 "$1" | cut -d' ' -f1; fi
}
sum1() {
    if command -v sha1sum >/dev/null 2>&1; then sha1sum "$1" | cut -d' ' -f1; else shasum -a 1 "$1" | cut -d' ' -f1; fi
}

# 설치된 NDK 폴더 하나의 판정: 고정 판이고 clang이 있으면 0.
ndk_ok() {
    local dir=$1 got
    got=$(revision_of "$dir")
    if [ "$got" != "$NDK_REVISION" ]; then
        say "${dir}의 판이 '${got:-source.properties 없음}'이다(기대 $NDK_REVISION)."
        return 1
    fi
    if [ ! -f "$dir/$CLANG_REL" ] || [ ! -x "$dir/$CLANG_REL" ]; then
        say "${dir}에 실행 파일 ${CLANG_REL}이 없다."
        return 1
    fi
}

# 끝날 때 지울 것: 이 실행의 임시 폴더와, 이 실행이 만들었지만 다 채우지 못한 목적지. 경로를 trap 글자에
# 넣지 않고 변수로 들고 있으므로 PREFIX에 따옴표가 들어도 된다.
INSTALL_TMP=""
CLAIMED_DEST=""
cleanup() {
    if [ -n "$CLAIMED_DEST" ]; then rm -rf -- "${CLAIMED_DEST:?}"; fi
    if [ -n "$INSTALL_TMP" ]; then rm -rf -- "${INSTALL_TMP:?}"; fi
}

install_main() {
    local prefix="${PREFIX:-$HOME/.local/share/android-ndk}" dest tmp got e
    dest="$prefix/$NAME"

    if [ -e "$dest" ] || [ -L "$dest" ]; then
        if [ -d "$dest" ] && ndk_ok "$dest"; then
            say "이미 있다: $dest (Pkg.Revision $NDK_REVISION)"
            echo "$dest"
            return 0
        fi
        say "${dest}가 이미 있지만 고정 판의 NDK가 아니다. 그 자리는 그대로 두고 멈춘다. 다른 PREFIX를 준다."
        return 2
    fi

    if ! host_ok; then
        say "이 스크립트는 Linux x86_64용이다(지금 $(uname -s) $(uname -m)). 다른 호스트는 Android SDK의 sdkmanager로"
        say "\"ndk;$NDK_REVISION\"을 받는다."
        return 2
    fi

    # 자체 시험은 이 함수를 errexit가 꺼지는 자리에서 부르므로 명령마다 종료 코드를 본다.
    mkdir -p "$prefix" || return 2
    tmp=$(mktemp -d "$prefix/.install-ndk.XXXXXX") || return 2
    INSTALL_TMP=$tmp
    trap cleanup EXIT

    say "받는다: $URL"
    fetch "$tmp/$ZIP" || { say "내려받기 실패."; return 2; }
    got=$(sum256 "$tmp/$ZIP")
    [ "$got" = "$NDK_SHA256" ] || { say "SHA-256이 다르다: $got (기대 $NDK_SHA256)"; return 2; }
    got=$(sum1 "$tmp/$ZIP")
    [ "$got" = "$NDK_SHA1" ] || { say "SHA-1이 다르다: $got (기대 $NDK_SHA1)"; return 2; }
    say "SHA-256·SHA-1 대조 통과"

    # 실행 권한과 심볼릭 링크를 지키는 도구로 푼다.
    mkdir "$tmp/x" || return 2
    if command -v unzip >/dev/null 2>&1; then
        unzip -q "$tmp/$ZIP" -d "$tmp/x" || { say "압축 풀기 실패."; return 2; }
    elif command -v bsdtar >/dev/null 2>&1; then
        bsdtar -xf "$tmp/$ZIP" -C "$tmp/x" || { say "압축 풀기 실패."; return 2; }
    else
        say "unzip도 bsdtar도 없다."
        return 2
    fi
    [ -d "$tmp/x/$NAME" ] || { say "아카이브 안에 $NAME 폴더가 없다."; return 2; }
    ndk_ok "$tmp/x/$NAME" || { say "푼 NDK가 고정 판의 NDK가 아니다."; return 2; }

    # 목적지는 mkdir로 이 실행이 만들 때만 쓴다. 그 사이 무언가 생겼으면(빈 폴더 포함, 같은 PREFIX의
    # 동시 설치) 실패하고 그 자리는 건드리지 않는다. 만든 뒤에는 푼 NDK의 항목을 그 안으로 옮긴다(같은
    # 파일 시스템 안의 이름 바꾸기). 다른 실행의 ndk_ok가 읽는 source.properties는 나머지를 다 옮기고
    # clang을 확인한 뒤 맨 마지막에 옮긴다. 그 전에는 다른 실행이 이 목적지를 받아들이지 않고 2로 끝나므로,
    # 그때까지는 실패하면 cleanup이 만든 목적지를 지워도 된다. source.properties를 옮기는 순간부터는
    # 지우지 않는다.
    if ! mkdir -- "$dest" 2>/dev/null; then
        say "${dest}를 만들지 못했다(그 사이 같은 자리에 무언가 생겼거나 만들 수 없다). 그 자리는 그대로 두고 멈춘다."
        return 2
    fi
    CLAIMED_DEST=$dest
    for e in "$tmp/x/$NAME"/* "$tmp/x/$NAME"/.[!.]* "$tmp/x/$NAME"/..?*; do
        [ -e "$e" ] || [ -L "$e" ] || continue
        [ "$e" = "$tmp/x/$NAME/source.properties" ] && continue
        mv -- "$e" "$dest/" || { say "${dest}로 옮기지 못했다."; return 2; }
    done
    if [ -e "$dest/$NAME" ] || [ -e "$dest/source.properties" ] \
        || [ ! -f "$dest/$CLANG_REL" ] || [ ! -x "$dest/$CLANG_REL" ]; then
        say "옮긴 뒤 ${dest}가 고정 판의 NDK가 아니다."
        return 2
    fi
    CLAIMED_DEST=""
    if ! mv -- "$tmp/x/$NAME/source.properties" "$dest/source.properties"; then
        say "${dest}로 source.properties를 옮기지 못했다."
        # 이동이 실패로 보여도 그 파일이 목적지에 있으면 다른 실행이 이미 받았을 수 있어 그대로 둔다.
        if [ ! -e "$dest/source.properties" ] && [ ! -L "$dest/source.properties" ]; then
            rm -rf -- "${dest:?}"
        fi
        return 2
    fi
    if ! ndk_ok "$dest"; then
        say "옮긴 뒤 ${dest}가 고정 판의 NDK가 아니다. 다른 실행이 이미 받았을 수 있어 그 자리는 그대로 둔다."
        return 2
    fi
    say "설치했다: $dest (Pkg.Revision $NDK_REVISION)"
    echo "$dest"
}

# 자체 시험: 가짜 NDK 아카이브(source.properties와 clang 자리의 실행 파일)를 만들어 받기를 그 복사로
# 바꾸고, PREFIX의 상태마다 종료 코드와 찍은 폴더를 본다.
self_test() {
    local work failed=0 c name want setup archive out got pfx left
    command -v python3 >/dev/null 2>&1 || { say "자체 시험에는 python3가 필요하다."; return 2; }
    command -v unzip >/dev/null 2>&1 || command -v bsdtar >/dev/null 2>&1 \
        || { say "자체 시험에는 unzip이나 bsdtar가 필요하다."; return 2; }
    work=$(mktemp -d "${TMPDIR:-/tmp}/install-ndk-test.XXXXXX")
    # 아카이브 종류: good(고정 판, clang 있음), noclang(clang 없음), oldrev(다른 판), noname(폴더 이름이 다름),
    # clangdir(clang 자리가 폴더)
    python3 -I - "$work" "$NAME" "$NDK_REVISION" "$CLANG_REL" <<'PY'
import sys, zipfile
work, name, rev, clang = sys.argv[1:5]
def make(kind):
    top = "android-ndk-other" if kind == "noname" else name
    r = "29.0.1" if kind == "oldrev" else rev
    with zipfile.ZipFile(f"{work}/{kind}.zip", "w") as z:
        z.writestr(f"{top}/source.properties", f"Pkg.Desc = Android NDK\nPkg.Revision = {r}\n")
        # 실제 아카이브처럼 toolchains 뒤에 옮길 항목(wrap.sh)을 둔다.
        z.writestr(f"{top}/wrap.sh", "#!/bin/sh\n")
        if kind == "clangdir":
            info = zipfile.ZipInfo(f"{top}/{clang}/")
            info.external_attr = (0o40755 << 16)
            z.writestr(info, "")
        elif kind != "noclang":
            info = zipfile.ZipInfo(f"{top}/{clang}")
            info.external_attr = (0o100755 << 16)
            z.writestr(info, "#!/bin/sh\n")
for k in ("good", "noclang", "oldrev", "noname", "clangdir"):
    make(k)
PY
    # 이름#기대 종료 코드#PREFIX 준비(fresh·again·emptydir·otherfiles·file·badrev·noclangdest·clangdirdest·
    # bad-sum·race·race-empty·quote·filling·filling-fail)#아카이브
    # (race는 받는 동안 목적지에 파일이 든 폴더가, race-empty는 빈 폴더가 생긴다. quote는 PREFIX에 작은따옴표가
    # 든다. filling은 목적지를 채우는 중(wrap.sh를 옮기기 직전)에 같은 PREFIX로 다른 호출을 하고, 그
    # 호출이 2가 아니면 실패다. filling-fail은 거기에 더해 wrap.sh 옮기기가 실패한다. 어느 경우든 끝난 뒤
    # PREFIX에 임시 폴더 .install-ndk.*가 남으면 실패다. props-moved-fail은 source.properties 이동이
    # 이름을 바꾼 뒤 실패로 끝난 것처럼 하고 그 사이 다른 호출을 한다. 다른 호출이 0이 아니거나 목적지가
    # 지워지면 실패다)
    local cases=(
        "빈 PREFIX에 설치한다#0#fresh#good"
        "같은 판이 이미 있다#0#again#good"
        "목적지가 빈 폴더다#2#emptydir#good"
        "목적지에 source.properties 없는 파일이 있다#2#otherfiles#good"
        "목적지가 파일이다#2#file#good"
        "목적지가 다른 판이다#2#badrev#good"
        "목적지의 판은 맞는데 clang이 없다#2#noclangdest#good"
        "아카이브에 clang이 없다#2#fresh#noclang"
        "아카이브가 다른 판이다#2#fresh#oldrev"
        "아카이브에 그 이름의 폴더가 없다#2#fresh#noname"
        "아카이브의 해시가 다르다#2#bad-sum#good"
        "받는 사이 다른 설치가 목적지를 만든다#2#race#good"
        "받는 사이 목적지에 빈 폴더가 생긴다#2#race-empty#good"
        "목적지의 clang 자리가 폴더다#2#clangdirdest#good"
        "아카이브의 clang 자리가 폴더다#2#fresh#clangdir"
        "PREFIX에 작은따옴표가 든다#0#quote#good"
        "채우는 중인 목적지를 다른 호출이 본다#0#filling#good"
        "채우는 중 실패하고 다른 호출이 그 목적지를 본다#2#filling-fail#good"
        "source.properties 이동이 끝난 뒤 실패로 보이고 다른 호출이 그 목적지를 받는다#2#props-moved-fail#good"
    )
    for c in "${cases[@]}"; do
        IFS='#' read -r name want setup archive <<<"$c"
        rm -rf "${work:?}/p" "${work:?}/p'q"
        pfx="$work/p"
        [ "$setup" = quote ] && pfx="$work/p'q"
        mkdir -p "$pfx"
        local dest="$pfx/$NAME"
        case "$setup" in
            again)
                mkdir -p "$dest/$(dirname "$CLANG_REL")"
                printf 'Pkg.Revision = %s\n' "$NDK_REVISION" >"$dest/source.properties"
                printf '#!/bin/sh\n' >"$dest/$CLANG_REL"
                chmod +x "$dest/$CLANG_REL"
                ;;
            emptydir) mkdir "$dest" ;;
            otherfiles) mkdir "$dest" && : >"$dest/README" ;;
            file) : >"$dest" ;;
            badrev) mkdir "$dest" && printf 'Pkg.Revision = 29.0.1\n' >"$dest/source.properties" ;;
            noclangdest) mkdir "$dest" && printf 'Pkg.Revision = %s\n' "$NDK_REVISION" >"$dest/source.properties" ;;
            clangdirdest)
                mkdir -p "$dest/$CLANG_REL"
                printf 'Pkg.Revision = %s\n' "$NDK_REVISION" >"$dest/source.properties"
                ;;
        esac
        got=0
        out=$(
            # shellcheck disable=SC2329  # install_main이 부른다
            host_ok() { return 0; }
            # shellcheck disable=SC2329  # install_main이 부른다
            # shellcheck disable=SC2329  # install_main이 부른다
            mv() {
                local a
                for a in "$@"; do
                    case "$a" in
                        (*/source.properties)
                            if [ "$setup" = props-moved-fail ]; then
                                command mv "$@" || return 1
                                (INSTALL_TMP="" CLAIMED_DEST="" PREFIX="$pfx" install_main >/dev/null 2>&1)
                                echo "$?" >"$work/other.rc"
                                return 1
                            fi
                            ;;
                        (*/wrap.sh)
                            if [ "$setup" = filling ] || [ "$setup" = filling-fail ]; then
                                (INSTALL_TMP="" CLAIMED_DEST="" PREFIX="$pfx" install_main >/dev/null 2>&1)
                                echo "$?" >"$work/other.rc"
                                [ "$setup" = filling-fail ] && return 1
                            fi
                            ;;
                    esac
                done
                command mv "$@"
            }
            fetch() {
                cp "$work/$archive.zip" "$1"
                if [ "$setup" = race ]; then mkdir "$dest" && : >"$dest/partial"; fi
                if [ "$setup" = race-empty ]; then mkdir "$dest"; fi
            }
            NDK_SHA256=$(sum256 "$work/$archive.zip")
            NDK_SHA1=$(sum1 "$work/$archive.zip")
            [ "$setup" = bad-sum ] && NDK_SHA1=0000000000000000000000000000000000000000
            PREFIX="$pfx" install_main 2>/dev/null
        ) || got=$?
        if [ "$got" = 0 ] && { [ "$out" != "$dest" ] || [ "$(revision_of "$dest")" != "$NDK_REVISION" ] \
            || [ ! -f "$dest/$CLANG_REL" ] || [ ! -x "$dest/$CLANG_REL" ] || [ -e "$dest/$NAME" ]; }; then
            got="0(찍은 폴더 '$out'이 고정 판의 NDK가 아님)"
        fi
        if [ "$setup" = props-moved-fail ]; then
            if [ "$(cat "$work/other.rc" 2>/dev/null)" != 0 ]; then
                got="$got(이동 뒤 다른 호출의 결과 $(cat "$work/other.rc" 2>/dev/null), 기대 0)"
            fi
            if [ "$(revision_of "$dest")" != "$NDK_REVISION" ] || [ ! -f "$dest/$CLANG_REL" ]; then
                got="$got(다른 호출이 받은 목적지가 지워짐)"
            fi
            rm -f "$work/other.rc"
        fi
        if [ "$setup" = filling ] || [ "$setup" = filling-fail ]; then
            if [ "$(cat "$work/other.rc" 2>/dev/null)" != 2 ]; then
                got="$got(채우는 중 다른 호출의 결과 $(cat "$work/other.rc" 2>/dev/null), 기대 2)"
            fi
            rm -f "$work/other.rc"
        fi
        left=$(find "$pfx" -maxdepth 1 -name '.install-ndk.*' | wc -l)
        [ "$left" -eq 0 ] || got="$got(임시 폴더 ${left}개가 남음)"
        if [ "$got" = "$want" ]; then
            say "자체 시험 통과: $name"
        else
            say "자체 시험 실패: $name (기대 $want, 결과 $got)"
            failed=1
        fi
    done
    rm -rf "${work:?}"
    return "$failed"
}

if [ "${1:-}" = "--self-test" ]; then
    [ $# -eq 1 ] || { say "사용: bash scripts/install-ndk.sh [--self-test]"; exit 2; }
    self_test
    exit $?
fi
[ $# -eq 0 ] || { say "사용: PREFIX=<폴더> bash scripts/install-ndk.sh [--self-test]"; exit 2; }
install_main
