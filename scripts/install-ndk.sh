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
        say "$dir의 판이 '${got:-source.properties 없음}'이다(기대 $NDK_REVISION)."
        return 1
    fi
    if [ ! -x "$dir/$CLANG_REL" ]; then
        say "$dir에 $CLANG_REL이 없다."
        return 1
    fi
}

install_main() {
    local prefix="${PREFIX:-$HOME/.local/share/android-ndk}" dest tmp got
    dest="$prefix/$NAME"

    if [ -e "$dest" ] || [ -L "$dest" ]; then
        if [ -d "$dest" ] && ndk_ok "$dest"; then
            say "이미 있다: $dest (Pkg.Revision $NDK_REVISION)"
            echo "$dest"
            return 0
        fi
        say "$dest가 이미 있지만 고정 판의 NDK가 아니다. 그 자리는 그대로 두고 멈춘다. 다른 PREFIX를 준다."
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
    # shellcheck disable=SC2064  # tmp는 지금 값으로 묶는다
    trap "rm -rf '${tmp:?}'" EXIT

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

    # -T: dest가 그 사이 생겼으면 그 아래로 넣지 않고 실패한다(같은 PREFIX의 동시 설치).
    if ! mv -T "$tmp/x/$NAME" "$dest"; then
        say "$dest로 옮기지 못했다(그 사이 같은 자리에 무언가 생겼을 수 있다). 그 자리는 그대로 두고 멈춘다."
        return 2
    fi
    if [ -e "$dest/$NAME" ] || ! ndk_ok "$dest"; then
        say "옮긴 뒤 $dest가 고정 판의 NDK가 아니다."
        return 2
    fi
    say "설치했다: $dest (Pkg.Revision $NDK_REVISION)"
    echo "$dest"
}

# 자체 시험: 가짜 NDK 아카이브(source.properties와 clang 자리의 실행 파일)를 만들어 받기를 그 복사로
# 바꾸고, PREFIX의 상태마다 종료 코드와 찍은 폴더를 본다.
self_test() {
    local work failed=0 c name want setup archive out got
    command -v python3 >/dev/null 2>&1 || { say "자체 시험에는 python3가 필요하다."; return 2; }
    command -v unzip >/dev/null 2>&1 || command -v bsdtar >/dev/null 2>&1 \
        || { say "자체 시험에는 unzip이나 bsdtar가 필요하다."; return 2; }
    work=$(mktemp -d "${TMPDIR:-/tmp}/install-ndk-test.XXXXXX")
    # 아카이브 종류: good(고정 판, clang 있음), noclang(clang 없음), oldrev(다른 판), noname(폴더 이름이 다름)
    python3 -I - "$work" "$NAME" "$NDK_REVISION" "$CLANG_REL" <<'PY'
import sys, zipfile
work, name, rev, clang = sys.argv[1:5]
def make(kind):
    top = "android-ndk-other" if kind == "noname" else name
    r = "29.0.1" if kind == "oldrev" else rev
    with zipfile.ZipFile(f"{work}/{kind}.zip", "w") as z:
        z.writestr(f"{top}/source.properties", f"Pkg.Desc = Android NDK\nPkg.Revision = {r}\n")
        if kind != "noclang":
            info = zipfile.ZipInfo(f"{top}/{clang}")
            info.external_attr = (0o100755 << 16)
            z.writestr(info, "#!/bin/sh\n")
for k in ("good", "noclang", "oldrev", "noname"):
    make(k)
PY
    # 이름#기대 종료 코드#PREFIX 준비(fresh·again·emptydir·otherfiles·file·badrev·noclangdest·bad-sum·race)#아카이브
    # (race는 받는 동안 목적지에 파일이 든 폴더가 생긴다)
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
    )
    for c in "${cases[@]}"; do
        IFS='#' read -r name want setup archive <<<"$c"
        rm -rf "${work:?}/p"
        mkdir -p "$work/p"
        local dest="$work/p/$NAME"
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
        esac
        got=0
        out=$(
            # shellcheck disable=SC2329  # install_main이 부른다
            host_ok() { return 0; }
            # shellcheck disable=SC2329  # install_main이 부른다
            fetch() {
                cp "$work/$archive.zip" "$1"
                if [ "$setup" = race ]; then mkdir "$dest" && : >"$dest/partial"; fi
            }
            NDK_SHA256=$(sum256 "$work/$archive.zip")
            NDK_SHA1=$(sum1 "$work/$archive.zip")
            [ "$setup" = bad-sum ] && NDK_SHA1=0000000000000000000000000000000000000000
            PREFIX="$work/p" install_main 2>/dev/null
        ) || got=$?
        if [ "$got" = 0 ] && { [ "$out" != "$dest" ] || [ "$(revision_of "$dest")" != "$NDK_REVISION" ] \
            || [ ! -x "$dest/$CLANG_REL" ] || [ -e "$dest/$NAME" ]; }; then
            got="0(찍은 폴더 '$out'이 고정 판의 NDK가 아님)"
        fi
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
