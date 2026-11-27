#!/usr/bin/env bash
# SSH 통합 시험(crates/tasty-net-demo/tests/ssh_attach.rs, ssh_reverse_attach.rs)이 쓸 OpenSSH sshd를 확인하고 결과를 로그에 남긴다.
# CI의 clippy·test 잡이 시험 전에 부른다(docs/dev-guide/ci.md). 로컬에서도 같은 명령으로 돌린다.
#
#   bash scripts/prepare-test-sshd.sh
#
# sshd를 찾으면 임시 폴더에 호스트 키와 시험 도우미와 같은 설정을 만들어 `sshd -t -f <설정>`으로 검사한다.
# 결과는 `sshd-prep ...` 줄로 남긴다. sshd가 없거나 검사가 실패해도 종료 코드는 0이다. 그때 시험은
# 시험 전용 Rust SSH 서버로 돌고, 그것도 안 되면 시험이 sshd=none으로 실패한다(docs/design/testing.md
# "SSH 통합 시험의 sshd"). 이 단계는 시험을 통과시키거나 건너뛰게 하지 않는다.
# TASTY_NET_TEST_SSHD_PATH로 sshd 경로를 정할 수 있다(시험 도우미와 같다).
set -u -o pipefail

say() { echo "sshd-prep $*"; }

os="$(uname -s)"
find_sshd() {
    if [ -n "${TASTY_NET_TEST_SSHD_PATH:-}" ]; then
        echo "$TASTY_NET_TEST_SSHD_PATH"
        return 0
    fi
    local c
    case "$os" in
        MINGW* | MSYS* | CYGWIN*)
            # Git Bash의 /usr/bin/sshd(MSYS)는 쓰지 않는다. 시험 도우미가 찾는 Windows OpenSSH 자리만 본다.
            for c in "${WINDIR:-C:/Windows}/System32/OpenSSH/sshd.exe" "${ProgramFiles:-C:/Program Files}/OpenSSH/sshd.exe"; do
                [ -f "$c" ] && { echo "$c"; return 0; }
            done
            ;;
        *)
            for c in /usr/sbin/sshd /usr/local/sbin/sshd /opt/homebrew/sbin/sshd; do
                [ -x "$c" ] && { echo "$c"; return 0; }
            done
            ;;
    esac
    return 1
}

if ! sshd="$(find_sshd)"; then
    say "os=$os sshd=missing (the SSH tests fall back to the test-only Rust server)"
    exit 0
fi
version="$("$sshd" -V 2>&1 | head -1 || true)"
say "os=$os sshd=$sshd version=${version:-unknown}"

keygen=ssh-keygen
case "$os" in
    MINGW* | MSYS* | CYGWIN*)
        w="${WINDIR:-C:/Windows}/System32/OpenSSH/ssh-keygen.exe"
        [ -f "$w" ] && keygen="$w"
        ;;
esac

dir="$(mktemp -d)" || { say "config-check=failed (no temporary directory)"; exit 0; }
trap 'rm -rf "${dir:?}"' EXIT
# Windows의 sshd.exe는 설정 파일 안의 MSYS 경로(/tmp/...)를 모른다. C:/... 형태로 적는다.
cdir="$dir"
command -v cygpath >/dev/null 2>&1 && cdir="$(cygpath -m "$dir")"
if ! "$keygen" -q -t ed25519 -N "" -C tasty-net-test -f "$dir/host_key" >"$dir/keygen.log" 2>&1; then
    say "config-check=failed (ssh-keygen: $(tr '\n' ' ' <"$dir/keygen.log"))"
    exit 0
fi
: >"$dir/authorized_keys"

# 시험 도우미(crates/tasty-net-demo/tests/support/sshd.rs)의 sshd_config와 같은 줄이다.
write_config() {
    {
        echo "Port 2222"
        echo "ListenAddress 127.0.0.1"
        echo "HostKey \"$cdir/host_key\""
        echo "AuthorizedKeysFile \"$cdir/authorized_keys\""
        echo "StrictModes no"
        echo "AllowTcpForwarding yes"
        echo "PubkeyAuthentication yes"
        echo "PasswordAuthentication no"
        echo "KbdInteractiveAuthentication no"
        case "$os" in
            MINGW* | MSYS* | CYGWIN*) ;;
            *)
                echo "PidFile \"$cdir/sshd.pid\""
                echo "UsePAM no"
                ;;
        esac
        [ "$1" = yes ] && echo "PerSourcePenalties no"
    } >"$dir/sshd_config"
}

for penalties in yes no; do
    write_config "$penalties"
    if "$sshd" -t -f "$dir/sshd_config" >"$dir/check.log" 2>&1; then
        say "config-check=ok PerSourcePenalties=$([ "$penalties" = yes ] && echo set || echo unsupported)"
        exit 0
    fi
done
say "config-check=failed ($(tr '\n' ' ' <"$dir/check.log"))"
exit 0
