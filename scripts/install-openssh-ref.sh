#!/usr/bin/env bash
# ssh -G·ssh-keygen -F 대조 시험(crates/tasty-net-ssh-config/tests/)의 기준 클라이언트를
# 이식 기준 커밋(OpenSSH portable 10.6p2)에서 빌드한다(docs/design/ssh-backends.md
# "비교 시험의 기준 클라이언트"). ssh와 ssh-keygen만 만들어 <PREFIX>/openssh-10.6p2/bin/에 둔다.
# make install은 하지 않는다(시스템 설정 폴더에 호스트 키를 만들기 때문이다).
#
# 사용: bash scripts/install-openssh-ref.sh
#   PREFIX                    설치 폴더의 부모(기본 <레포>/target/tools)
#   OPENSSH_CONFIGURE_EXTRA   configure에 더할 인자(예: macOS의 --with-ssl-dir=<openssl 폴더>)
# 필요한 도구: git, autoreconf(autoconf), make, C 컴파일러, OpenSSL·zlib·Kerberos 개발 파일.
# 이미 같은 판이 있으면 다시 빌드하지 않는다. 종료 코드: 성공 0, 실패 1, 도구 없음 2.

set -e -o pipefail

cd "$(dirname "$0")/.."

RELEASE="OpenSSH_10.6p2"
REPO_URL="https://github.com/openssh/openssh-portable"
COMMIT="d449e662b6b8b8cb20e852d8084c269af1c5e8f1"
PREFIX="${PREFIX:-$PWD/target/tools}"
DEST="$PREFIX/openssh-10.6p2"
TAG="[install-openssh-ref]"

version_of() {
    "$1" -V 2>&1 | head -n 1
}

if [ -x "$DEST/bin/ssh" ] && [ -x "$DEST/bin/ssh-keygen" ]; then
    v="$(version_of "$DEST/bin/ssh")"
    case "$v" in
        "$RELEASE"*) echo "$TAG 이미 있다: $DEST/bin/ssh ($v)"; exit 0 ;;
        *) echo "$TAG $DEST/bin/ssh 의 판이 다르다($v) — 다시 빌드한다." ;;
    esac
fi

for tool in git autoreconf make cc; do
    command -v "$tool" >/dev/null 2>&1 || {
        echo "$TAG 판정 불가: $tool 이 PATH에 없다." >&2
        echo "  Debian/Ubuntu: sudo apt-get install -y autoconf build-essential libssl-dev zlib1g-dev libkrb5-dev" >&2
        echo "  macOS: brew install autoconf automake openssl@3" >&2
        exit 2
    }
done

TMP="$(mktemp -d)"
trap 'rm -rf "${TMP:?}"' EXIT

echo "$TAG 받는다: $REPO_URL $COMMIT"
git -C "$TMP" init -q src
git -C "$TMP/src" fetch -q --depth 1 "$REPO_URL" "$COMMIT"
GOT="$(git -C "$TMP/src" rev-parse FETCH_HEAD)"
[ "$GOT" = "$COMMIT" ] || {
    echo "$TAG 받은 커밋이 다르다 — 빌드하지 않는다(기대 $COMMIT, 실제 $GOT)." >&2
    exit 1
}
git -C "$TMP/src" checkout -q --detach FETCH_HEAD

# 설계의 분류가 기준으로 삼는 갈래: GSSAPI(Kerberos)와 PKCS#11을 켠 빌드. 경로 기본값은
# 배포판 OpenSSH와 같은 /etc/ssh, /usr/bin/xauth로 둔다(ssh -G 출력에 나온다).
# shellcheck disable=SC2086
(
    cd "$TMP/src"
    autoreconf
    ./configure -q --prefix="$DEST" --sysconfdir=/etc/ssh --with-xauth=/usr/bin/xauth \
        --with-kerberos5 --enable-pkcs11 ${OPENSSH_CONFIGURE_EXTRA:-}
    make -s ssh ssh-keygen
)

grep -Eq '^#define GSSAPI( |$)' "$TMP/src/config.h" || {
    echo "$TAG GSSAPI가 꺼진 빌드다(Kerberos 개발 파일을 확인하라) — 놓지 않는다." >&2
    exit 1
}
grep -Eq '^#define ENABLE_PKCS11( |$)' "$TMP/src/config.h" || {
    echo "$TAG PKCS#11이 꺼진 빌드다 — 놓지 않는다." >&2
    exit 1
}

mkdir -p "$DEST/bin"
install -m 0755 "$TMP/src/ssh" "$DEST/bin/ssh"
install -m 0755 "$TMP/src/ssh-keygen" "$DEST/bin/ssh-keygen"
v="$(version_of "$DEST/bin/ssh")"
case "$v" in
    "$RELEASE"*) echo "$TAG 놓았다: $DEST/bin/ssh ($v)" ;;
    *) echo "$TAG 빌드한 판이 다르다($v)." >&2; exit 1 ;;
esac
