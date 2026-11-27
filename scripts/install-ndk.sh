#!/usr/bin/env bash
# 고정 판의 Android NDK(Linux x86_64)를 받아 PREFIX 아래에 푼다. 이미 같은 판이 있으면 받지 않는다.
# 마지막 줄(stdout)에 NDK 폴더를 찍는다. 진행 줄은 stderr로 간다. CI의 mobile-android 잡이 쓴다.
#
# 사용: PREFIX=<폴더> bash scripts/install-ndk.sh      (기본 PREFIX: $HOME/.local/share/android-ndk)
#       ndk=$(PREFIX=<폴더> bash scripts/install-ndk.sh | tail -n 1)
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

PREFIX="${PREFIX:-$HOME/.local/share/android-ndk}"
NAME="android-ndk-$NDK_RELEASE"
ZIP="$NAME-linux.zip"
URL="https://dl.google.com/android/repository/$ZIP"
DEST="$PREFIX/$NAME"

say() { echo "[install-ndk] $*" >&2; }

revision_of() {
    sed -n 's/^Pkg\.Revision *= *//p' "$1/source.properties" 2>/dev/null | tr -d '\r'
}

if [ -f "$DEST/source.properties" ]; then
    got=$(revision_of "$DEST")
    if [ "$got" = "$NDK_REVISION" ]; then
        say "이미 있다: $DEST (Pkg.Revision $got)"
        echo "$DEST"
        exit 0
    fi
    say "$DEST의 판이 $got이다(기대 $NDK_REVISION). 그 폴더는 그대로 두고 멈춘다. 다른 PREFIX를 준다."
    exit 2
fi

if [ "$(uname -s)" != Linux ] || [ "$(uname -m)" != x86_64 ]; then
    say "이 스크립트는 Linux x86_64용이다(지금 $(uname -s) $(uname -m)). 다른 호스트는 Android SDK의 sdkmanager로"
    say "\"ndk;$NDK_REVISION\"을 받는다."
    exit 2
fi

sum256() {
    if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1" | cut -d' ' -f1; else shasum -a 256 "$1" | cut -d' ' -f1; fi
}
sum1() {
    if command -v sha1sum >/dev/null 2>&1; then sha1sum "$1" | cut -d' ' -f1; else shasum -a 1 "$1" | cut -d' ' -f1; fi
}

mkdir -p "$PREFIX"
TMP=$(mktemp -d "$PREFIX/.install-ndk.XXXXXX")
trap 'rm -rf "${TMP:?}"' EXIT

say "받는다: $URL"
curl -fsSL --retry 3 -o "$TMP/$ZIP" "$URL" || { say "내려받기 실패."; exit 2; }
got=$(sum256 "$TMP/$ZIP")
[ "$got" = "$NDK_SHA256" ] || { say "SHA-256이 다르다: $got (기대 $NDK_SHA256)"; exit 2; }
got=$(sum1 "$TMP/$ZIP")
[ "$got" = "$NDK_SHA1" ] || { say "SHA-1이 다르다: $got (기대 $NDK_SHA1)"; exit 2; }
say "SHA-256·SHA-1 대조 통과"

# 실행 권한과 심볼릭 링크를 지키는 도구로 푼다.
if command -v unzip >/dev/null 2>&1; then
    unzip -q "$TMP/$ZIP" -d "$TMP/x"
elif command -v bsdtar >/dev/null 2>&1; then
    mkdir -p "$TMP/x"
    bsdtar -xf "$TMP/$ZIP" -C "$TMP/x"
else
    say "unzip도 bsdtar도 없다."
    exit 2
fi
[ -d "$TMP/x/$NAME" ] || { say "아카이브 안에 $NAME 폴더가 없다."; exit 2; }
got=$(revision_of "$TMP/x/$NAME")
[ "$got" = "$NDK_REVISION" ] || { say "푼 NDK의 Pkg.Revision이 $got이다(기대 $NDK_REVISION)."; exit 2; }
mv "$TMP/x/$NAME" "$DEST"
say "설치했다: $DEST (Pkg.Revision $got)"
echo "$DEST"
