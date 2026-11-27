#!/usr/bin/env bash
# 이미지로 띄운 rendezvous 서버에 기기 두 개를 붙이는 확인. 동작 수준 기준 D5 (b)다(원격 빌드 환경에서 돌린다).
#
#   rendezvous/container-check.sh                이미지를 빌드해 서버를 띄우고 p2p_peer 예제 두 개를 붙인다
#   rendezvous/container-check.sh --self-test    docker 없이 이 스크립트의 판정 함수만 시험한다
#
# 구성(같은 머신의 두 프로세스):
#   - 서버: rendezvous/Dockerfile로 빌드한 이미지. 기본 브리지 네트워크에서 0.0.0.0:3478·3479를 듣고,
#     호스트의 127.0.0.1 빈 UDP 포트 둘로 게시한다. 설정은 예시(rendezvous.toml)에서 두 줄만 바꾼다:
#     advertise.primary_ip = 127.0.0.1, advertise.alternate_port = 게시한 보조 포트.
#   - 기기: 호스트에서 cargo로 빌드한 p2p_peer 예제 두 프로세스(--listen, --dial). 서버 주소는
#     127.0.0.1:<게시한 주 포트>이고, 호스트 후보로 이 머신의 사설 IPv4(--host-ip)를 알린다.
#   - Docker가 게시 포트를 docker-proxy로 나르면(Linux Docker Engine 기본) 서버가 보는 기기 주소는
#     프록시 소켓의 주소다. 주·보조 포트가 다른 프록시 소켓을 거치므로 두 기기 모두 매핑이
#     dependent로 판별되고, 서버는 호스트 후보만 싣는다(mode host_only, docs/design/p2p.md "시도 규칙").
#     그래서 연결은 같은 공유기 안의 두 기기처럼 사설 IPv4 호스트 후보로 선다.
#     프록시를 끈 Docker(userland-proxy false)에서는 반사 주소가 이 머신의 주소가 되어 반사 후보로
#     붙을 수 있다. 그때 이 검사는 path 단계에서 실패하고 그 경로를 적는다.
#
# 검사 순서:
#   1. build: p2p_peer 예제를 빌드하고(P2P_PEER를 주면 그 파일), 이미지를 빌드한다(CONTAINER_CHECK_IMAGE를
#      주면 그 이미지). 실행 sha(git HEAD)를 적는다. 읽지 못하면 판정 불가다. build 줄과 result 줄에
#      image=built|given, peer=built|given을 적는다. D5 (b)의 증거는 image=built peer=built인 실행이다.
#   2. config: 예시 설정의 광고 두 줄을 바꾼 사본을 이미지의 settings --config로 읽어 두 값이 configured인지 본다.
#   3. keygen: 임시 볼륨에 서버 키를 만들고 공개키를 얻는다.
#   4. start: 컨테이너를 띄워 시작 줄(같은 공개키)을 기다린다.
#   5. connect-control, connect-data: 두 기기가 서버의 소개로 붙어 메시지를 echo한다. 대조 실행은 1바이트,
#      데이터 실행은 CONTAINER_CHECK_MESSAGE_BYTES 바이트다. 실행마다 앞뒤로 서버 쪽 수신 카운터(컨테이너
#      eth0의 rx_bytes·rx_packets)를 읽고, 두 예제의 연결 정보 줄(경로·주소쌍·서버)을 그대로 적는다.
#   6. path-control, path-data: 두 기기의 성공 후보 종류가 host이고 상대 주소가 알린 호스트 IP인지, 서버가
#      본 반사 주소와 다른지 본다. 서버 로그의 매핑 판별과 펀칭 mode도 적는다. 반사 주소·매핑·mode는 그 실행
#      구간의 서버 로그(실행 전 줄 수 뒤의 줄)에서만 읽는다.
#   7. no-forward: 데이터 실행의 서버 쪽 수신 바이트가 대조 실행의 두 배를 넘지 않는지 본다. 두 실행은 메시지
#      크기만 다르고 서버로 가는 제어 트래픽은 메시지를 싣지 않는다. 메시지가 서버를 지났다면 데이터 실행은
#      대조 실행 + 메시지 바이트 이상이다. 메시지가 대조 실행의 두 배 이하이거나 어느 실행의 수신 바이트·패킷
#      증가가 0이면 판정 불가다(근거: no_forward_ok 주석).
#   8. stop: docker stop(SIGINT)으로 멈추고 종료 코드 0과 종료 로그를 본다.
#   9. cleanup: 예제가 남았으면 SIGTERM 뒤 기한 안에 끝나는지 보고 남으면 SIGKILL로 끝내 거둔다. 이 실행의
#      컨테이너·볼륨·이미지를 지우고 남지 않았는지 본다. 하나라도 실패하면 result=fail step=cleanup이고
#      종료 코드 1이다. 결과 줄은 정리가 끝난 뒤에 쓴다. 실패·판정 불가로 끝나는 경로도 같다: 정리 뒤에
#      result=fail step=<단계>와 그 종료 코드(1 또는 2)를 쓰고, 정리가 실패하면 step=cleanup·종료 코드 1로 덮는다.
#  10. 결과를 rendezvous-check step=<단계> ok 줄들과 마지막 줄 rendezvous-check result=ok|fail로 쓴다.
#      result=ok 줄에 기기 구성(devices=same-host-processes), 경로, 실행 sha를 함께 적는다.
# 컨테이너·볼륨·이미지 이름은 실행마다 /dev/urandom에서 얻은 조각을 붙여 정하고, 만들기 전에 같은 이름이
# 없는지 본다. 셋 모두 그 조각을 값으로 한 라벨(tasty.container-check)을 달고, 지울 때는 라벨이 이 실행의
# 것일 때만 지운다. 컨테이너·볼륨·임시 폴더와 이 검사가 빌드한 이미지는 끝날 때 지운다.
#
# 필요한 것: Linux Docker Engine, cargo, python3(빈 포트 고르기), ip(사설 IPv4 찾기).
# 종료 코드: 0 통과, 1 실패, 2 판정 불가(도구 없음, 빌드 실패, 사설 IPv4 없음, 인자 오류).
#
# 환경 변수:
#   P2P_PEER                          p2p_peer 예제 실행 파일. 주면 빌드하지 않는다(peer=given)
#   CONTAINER_CHECK_IMAGE             서버 이미지. 주면 빌드하지 않고 지우지도 않는다(image=given)
#   CONTAINER_CHECK_HOST_IP           기기가 알릴 호스트 IP(사설 IPv4). 기본은 192.0.2.1로 가는 경로의 출발 IP
#   CONTAINER_CHECK_START_SECONDS     서버 시작 줄을 기다리는 시간(초, 1–3600). 기본 30
#   CONTAINER_CHECK_PEER_TIMEOUT      예제의 --timeout(초, 1–3600). 기본 60
#   CONTAINER_CHECK_STOP_SECONDS      docker stop이 SIGINT 뒤 기다리는 시간(초, 1–3600). 넘으면 SIGKILL하고 실패. 기본 15
#   CONTAINER_CHECK_MESSAGE_BYTES     echo할 메시지 크기(바이트, 1–120000). 기본 98304
set -uo pipefail
# 배열은 ${A[@]+"${A[@]}"}로 펼친다. bash 4.4 전(macOS의 3.2)은 set -u에서 빈 배열 "${A[@]}"를 오류로 본다.

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"
EXAMPLE="$HERE/rendezvous.toml"
START_SECONDS="${CONTAINER_CHECK_START_SECONDS:-30}"
PEER_TIMEOUT="${CONTAINER_CHECK_PEER_TIMEOUT:-60}"
STOP_SECONDS="${CONTAINER_CHECK_STOP_SECONDS:-15}"
MESSAGE_BYTES="${CONTAINER_CHECK_MESSAGE_BYTES:-98304}"
# 시간 설정(초)의 상한. 숫자 설정은 main_check 첫머리에서 decimal_in로 검사한 뒤에만 산술에 쓴다.
SECONDS_MAX=3600
# 예제는 --timeout 뒤 5초 안에 스스로 끝난다(예제의 WATCHDOG_GRACE). 그 뒤 이만큼 더 기다려도 남으면
# 실패로 끝낸다.
PEER_WAIT_EXTRA=15
# 기다리는 동안 다시 보는 간격(초).
POLL_SECONDS=0.2
# 메시지는 예제의 명령행 인자 하나로 넘긴다. Linux의 인자 하나 상한(128 KiB)보다 작게 둔다.
MESSAGE_BYTES_MAX=120000
# 컨테이너 안의 듣는 포트(예시 설정의 listen.* 기본값, Dockerfile의 EXPOSE).
CONTAINER_PRIMARY_PORT=3478
CONTAINER_ALTERNATE_PORT=3479
# 정리에서 예제에 SIGTERM을 보낸 뒤 끝나기를 기다리는 시간(초). 넘으면 SIGKILL로 끝낸다.
CLEANUP_SECONDS=5
# 기본 호스트 IP를 고를 때 경로를 묻는 대상(문서용 주소. 패킷은 보내지 않는다).
ROUTE_PROBE_IP=192.0.2.1

PEER_BUILD_CMD=(cargo build --locked --message-format=json-render-diagnostics -p tasty-net-p2p
    --example p2p_peer)

log() { echo "[rendezvous-check] $*" >&2; }

# 파일 머리의 주석 블록(둘째 줄부터 첫 비주석 줄 전까지)을 도움말로 출력한다.
show_help() { awk 'NR > 1 && !/^#/ { exit } NR > 1 { sub(/^# ?/, ""); print }' "$0"; }

# ── 판정 함수(자체 시험 대상) ────────────────────────────────────────────

# 줄 $1에서 "이름=값" 필드 $2의 값(다음 공백 전까지)을 낸다. 없으면 빈 값이다.
field_of() {
    sed -n "s/.*[[:space:]]$2=\([^[:space:]]*\).*/\1/p" <<<" $1" | head -n 1
}

# 주소 $1(IP:포트 또는 [IPv6]:포트)의 IP.
ip_of() {
    local a=${1%:*}
    a=${a#[}
    echo "${a%]}"
}

# $1이 사설 IPv4(RFC 1918)다. 기기가 호스트 후보로 알리는 IPv4는 이것뿐이다(rendezvous.md "후보 주소 규칙").
is_private_ipv4() {
    local a b c d
    [[ "$1" =~ ^([0-9]{1,3})\.([0-9]{1,3})\.([0-9]{1,3})\.([0-9]{1,3})$ ]] || return 1
    a=$((10#${BASH_REMATCH[1]}))
    b=$((10#${BASH_REMATCH[2]}))
    c=$((10#${BASH_REMATCH[3]}))
    d=$((10#${BASH_REMATCH[4]}))
    ((a <= 255 && b <= 255 && c <= 255 && d <= 255)) || return 1
    ((a == 10)) && return 0
    ((a == 172 && b >= 16 && b <= 31)) && return 0
    ((a == 192 && b == 168)) && return 0
    return 1
}

# 설정 파일 $1의 [advertise] 표에서 primary_ip 줄을 IP $2로, 주석 처리한 alternate_port 줄을 포트 $3으로
# 바꿔 낸다. 두 줄이 각각 정확히 하나 바뀌지 않으면 실패한다([listen]의 같은 이름 줄은 그대로 둔다).
advertise_config() {
    awk -v ip="$2" -v port="$3" '
        /^\[/ { table = $0 }
        table == "[advertise]" && /^primary_ip = / { print "primary_ip = \"" ip "\""; p++; next }
        table == "[advertise]" && /^# alternate_port = / { print "alternate_port = " port; a++; next }
        { print }
        END { if (p != 1 || a != 1) exit 1 }
    ' "$1"
}

# no_forward_ok <데이터 실행의 서버 수신 바이트> <대조 실행의 서버 수신 바이트> <메시지 바이트>
#               <데이터 실행의 서버 수신 패킷> <대조 실행의 서버 수신 패킷>
# 두 실행은 같은 단계(등록, NAT 판별, 펀칭 요청, 연결, echo, 닫기)를 밟고 메시지 크기만 다르다. 서버로 가는
# 제어 트래픽(조정 연결과 STUN)은 메시지를 싣지 않으므로 메시지 크기와 무관하다. 그 실행마다의 차이(연결 재요청,
# STUN 재전송, 펀칭 재시도 punch.attempts 2회)는 제어 교환 한 번을 더 하는 것을 넘지 않는다고 보고, 데이터
# 실행의 상한을 대조 실행의 두 배로 둔다. 메시지(한 방향만 해도 메시지 바이트)가 서버를 지났다면 데이터 실행은
# 대조 실행 + 메시지 바이트 이상이므로, 메시지가 대조 실행의 두 배보다 크면 상한을 넘는다.
# 두 실행 모두 기기가 서버에 등록·STUN·펀칭 요청을 하므로 서버 수신 바이트와 패킷이 함께 늘어야 한다. 어느
# 실행이든 바이트나 패킷 증가가 0이면(음수는 숫자 검사에서 걸린다) 그 구간의 카운터를 근거로 쓸 수 없다.
# 패킷 수는 이 유효성 검사에만 쓰고 상한에는 쓰지 않는다. 데이터그램 하나의 크기에 하한이 없어 패킷 수로는
# 메시지 바이트가 지났는지를 가를 수 없기 때문이다.
# 0: 상한 안. 1: 상한을 넘음(까닭 한 줄). 2: 판정 불가(값이 숫자가 아니거나, 어느 실행의 바이트·패킷 증가가
# 0이거나, 메시지가 대조 실행의 두 배 이하).
no_forward_ok() {
    local data=$1 control=$2 msg=$3 data_packets=${4-} control_packets=${5-} v
    for v in "$data" "$control" "$msg" "$data_packets" "$control_packets"; do
        if ! [[ "$v" =~ ^[0-9]{1,18}$ ]]; then
            echo "서버 수신 값이 십진수가 아니다: data='$data' control='$control' message='$msg' data_packets='$data_packets' control_packets='$control_packets'"
            return 2
        fi
    done
    if ((control == 0 || control_packets == 0)); then
        echo "대조 실행에서 서버 수신이 늘지 않았다(바이트 $control, 패킷 $control_packets). 카운터를 근거로 쓸 수 없다"
        return 2
    fi
    if ((data == 0 || data_packets == 0)); then
        echo "데이터 실행에서 서버 수신이 늘지 않았다(바이트 $data, 패킷 $data_packets). 카운터를 근거로 쓸 수 없다"
        return 2
    fi
    if ((msg <= 2 * control)); then
        echo "메시지($msg B)가 대조 실행 서버 수신($control B)의 두 배 이하라 판정할 수 없다. CONTAINER_CHECK_MESSAGE_BYTES를 키운다"
        return 2
    fi
    if ((data > 2 * control)); then
        echo "데이터 실행의 서버 수신($data B)이 상한(대조 실행 $control B의 두 배)을 넘었다"
        return 1
    fi
}

# path_ok <dialer 줄> <listener 줄> <호스트 IP> <서버가 본 반사 주소들(쉼표)>: 두 예제의 성공 후보 종류가
# host이고, 상대 주소의 IP가 알린 호스트 IP이며, 상대 주소가 서버가 본 반사 주소 어느 것과도 다르다.
# 맞으면 0. 아니면 까닭 한 줄을 내고 1이다. 반사 후보로 붙은 경우는 그 사실을 따로 적는다.
path_ok() {
    local dial_path listen_path dial_remote listen_remote
    dial_path=$(field_of "$1" path)
    listen_path=$(field_of "$2" path)
    dial_remote=$(field_of "$1" remote)
    listen_remote=$(field_of "$2" remote)
    if [[ "$dial_path" != host || "$listen_path" != host ]]; then
        case "$dial_path/$listen_path" in
            *reflexive*) echo "반사 후보로 붙었다(dialer path=$dial_path, listener path=$listen_path, 서버가 본 반사 주소 $4). 게시 포트를 docker-proxy가 나르지 않는 Docker로 보인다" ;;
            *) echo "성공 후보 종류가 host가 아니다: dialer path=$dial_path, listener path=$listen_path" ;;
        esac
        return 1
    fi
    if [[ "$(ip_of "$dial_remote")" != "$3" || "$(ip_of "$listen_remote")" != "$3" ]]; then
        echo "상대 주소가 알린 호스트 IP가 아니다: dialer remote=$dial_remote, listener remote=$listen_remote"
        return 1
    fi
    if [[ -z "$4" ]]; then
        echo "서버 로그에 기기의 반사 주소(registered addresses)가 없다"
        return 1
    fi
    case ",$4," in
        *",$dial_remote,"* | *",$listen_remote,"*)
            echo "연결 주소가 서버가 본 반사 주소와 같다: $4"
            return 1
            ;;
    esac
}

# 서버 로그 $1에서 기기마다 서버가 본 반사 주소(registered addresses)를 쉼표로 이어 낸다.
reflexive_of() {
    grep -F 'registered addresses' <<<"$1" | sed -n 's/.* reflexive=\([^ ]*\).*/\1/p' | sort -u | tr '\n' ',' | sed 's/,$//'
}

# log_after <로그> <줄 수>: 로그에서 앞의 <줄 수> 줄 뒤의 줄만 낸다. 실행마다 그 실행 전의 서버 로그 줄 수를
# 재 두고, 그 뒤의 줄로 그 실행의 반사 주소·매핑·mode를 읽는다(앞 실행의 줄을 쓰지 않는다).
log_after() {
    [[ "$2" =~ ^[0-9]{1,18}$ ]] || return 1
    awk -v n="$2" 'NR > n' <<<"$1"
}

# 글자 $1이 십진수이고 $2–$3 안이면 앞의 0을 뗀 값을 낸다. 아니면 실패한다. 산술식에 넣기 전에 형식과
# 자릿수(9자리까지)를 보므로 08 같은 값이 8진수로 읽히거나 큰 수가 넘쳐 감기지 않는다.
decimal_in() {
    local v
    [[ "$1" =~ ^[0-9]{1,9}$ ]] || return 1
    v=$((10#$1))
    ((v >= $2 && v <= $3)) || return 1
    echo "$v"
}

# 명령이 실패한다(자체 시험에서 "실패해야 하는" 사례를 적는다).
not() { ! "$@"; }

self_test() {
    local fails=0 n=0 dir got line
    check() {
        n=$((n + 1))
        if ! "$@"; then
            fails=$((fails + 1))
            log "자체 시험 실패: $*"
        fi
    }
    line='echo ok with abc via path=host local=10.255.0.1 local_reflexive=172.31.255.1:4000 remote=10.255.0.1:5000 server=127.0.0.1:3478'
    check test "$(field_of "$line" path)" = host
    check test "$(field_of "$line" remote)" = 10.255.0.1:5000
    check test "$(field_of "$line" local_reflexive)" = 172.31.255.1:4000
    check test -z "$(field_of "$line" missing)"
    check test "$(field_of 'echoed 3 bytes from x via path=server_reflexive remote=198.51.100.4:5' path)" = server_reflexive
    check test "$(ip_of 10.255.0.1:5000)" = 10.255.0.1
    check test "$(ip_of '[2001:db8::1]:5000')" = 2001:db8::1
    check is_private_ipv4 10.0.0.1
    check is_private_ipv4 172.16.0.1
    check is_private_ipv4 172.31.255.255
    check is_private_ipv4 192.168.255.254
    check not is_private_ipv4 172.32.0.1
    check not is_private_ipv4 127.0.0.1
    check not is_private_ipv4 203.0.113.10
    check not is_private_ipv4 192.168.255.256
    check not is_private_ipv4 2001:db8::1
    dir=$(mktemp -d) || { log "임시 폴더를 만들지 못했다"; return 2; }
    advertise_config "$EXAMPLE" 127.0.0.1 40001 >"$dir/ok.toml"
    check test "$?" -eq 0
    got=$(awk '/^\[/ { t = $0 } t == "[advertise]" && /^[a-z]/' "$dir/ok.toml")
    check test "$got" = $'primary_ip = "127.0.0.1"\nalternate_port = 40001'
    # [listen]의 주석 줄은 그대로다.
    got=$(awk '/^\[/ { t = $0 } t == "[listen]" && /^# alternate_port = /' "$dir/ok.toml")
    check test "$got" = '# alternate_port = 3479'
    grep -v '^primary_ip = ' "$EXAMPLE" >"$dir/no-ip.toml"
    check not advertise_config "$dir/no-ip.toml" 127.0.0.1 40001 >/dev/null
    sed 's/^# alternate_port = 3479$/alternate_port = 3479/' "$EXAMPLE" >"$dir/open.toml"
    check not advertise_config "$dir/open.toml" 127.0.0.1 40001 >/dev/null
    rm -rf "${dir:?}"
    no_forward_ok 20100 19800 98304 40 43 >/dev/null
    check test "$?" -eq 0
    no_forward_ok 39600 19800 98304 40 43 >/dev/null
    check test "$?" -eq 0
    no_forward_ok 39601 19800 98304 40 43 >/dev/null
    check test "$?" -eq 1
    no_forward_ok 118104 19800 98304 40 43 >/dev/null
    check test "$?" -eq 1
    no_forward_ok 20100 60000 98304 40 43 >/dev/null
    check test "$?" -eq 2
    no_forward_ok 20100 0 98304 40 43 >/dev/null
    check test "$?" -eq 2
    no_forward_ok '' 19800 98304 40 43 >/dev/null
    check test "$?" -eq 2
    no_forward_ok 0 19856 98304 40 43 >/dev/null
    check test "$?" -eq 2
    no_forward_ok 19589 19856 98304 0 43 >/dev/null
    check test "$?" -eq 2
    no_forward_ok 19589 19856 98304 39 0 >/dev/null
    check test "$?" -eq 2
    no_forward_ok 19589 19856 98304 -3 43 >/dev/null
    check test "$?" -eq 2
    no_forward_ok 19589 19856 98304 >/dev/null
    check test "$?" -eq 2
    check test "$(decimal_in 08 1 120000)" = 8
    check test "$(decimal_in 98304 1 120000)" = 98304
    check not decimal_in 0 1 120000
    check not decimal_in 120001 1 120000
    check not decimal_in 18446744073709649920 1 120000
    check not decimal_in -5 1 120000
    check not decimal_in '' 1 120000
    check not decimal_in 1e3 1 120000
    local d l r
    d='echo ok with b via path=host local=[::]:5001 local_reflexive=172.31.255.1:6001 remote=10.255.0.1:5002 server=127.0.0.1:3478'
    l='echoed 9 bytes from a via path=host local=10.255.0.1 local_reflexive=172.31.255.1:6002 remote=10.255.0.1:5001 server=127.0.0.1:3478'
    r=172.31.255.1:6001,172.31.255.1:6002
    check path_ok "$d" "$l" 10.255.0.1 "$r" >/dev/null
    check not path_ok "${d/path=host/path=server_reflexive}" "$l" 10.255.0.1 "$r" >/dev/null
    check test "$(path_ok "$d" "${l/path=host/path=reported_reflexive}" 10.255.0.1 "$r" | grep -c '반사 후보로 붙었다')" = 1
    check not path_ok "${d/path=host/path=unknown}" "$l" 10.255.0.1 "$r" >/dev/null
    check not path_ok "$d" "$l" 10.255.0.2 "$r" >/dev/null
    check not path_ok "${d/remote=10.255.0.1:5002/remote=10.255.0.9:5002}" "$l" 10.255.0.1 "$r" >/dev/null
    check not path_ok "$d" "$l" 10.255.0.1 "" >/dev/null
    check not path_ok "$d" "$l" 10.255.0.1 "172.31.255.1:6001,10.255.0.1:5001" >/dev/null
    check not path_ok "$d" "$l" 10.255.0.1 "10.255.0.1:5002" >/dev/null
    check test "$(reflexive_of $'DEBUG x: registered addresses device=a reflexive=172.31.255.1:6002 stun=[127.0.0.1:1]\nINFO x: registered device=a\nDEBUG x: registered addresses device=b reflexive=172.31.255.1:6001 stun=[]\nDEBUG x: registered addresses device=b reflexive=172.31.255.1:6001 stun=[]')" = 172.31.255.1:6001,172.31.255.1:6002
    check test -z "$(reflexive_of 'INFO x: registered device=a')"
    local clog dlog
    clog=$'DEBUG x: registered addresses device=a reflexive=172.31.255.1:6001 stun=[]\nINFO x: punch offered device=a peer=b punch_id=1 mode=HostOnly'
    dlog=$'DEBUG x: registered addresses device=a reflexive=172.31.255.1:7001 stun=[]'
    # 대조 실행 로그만 있고 데이터 실행 구간이 비면 반사 주소가 없어 path 판정이 실패한다.
    check test -z "$(log_after "$clog" 2)"
    check not path_ok "$d" "$l" 10.255.0.1 "$(reflexive_of "$(log_after "$clog" 2)")" >/dev/null
    # 데이터 실행 구간의 줄만 읽는다.
    check test "$(reflexive_of "$(log_after "$clog"$'\n'"$dlog" 2)")" = 172.31.255.1:7001
    check test "$(log_after "$clog" 0)" = "$clog"
    check not log_after "$clog" x
    # 실패·판정 불가 경로: 정리 뒤에 결과 줄을 쓰고, 정리가 실패하면 step=cleanup·rc 1로 덮는다.
    local out rc
    out=$(release() { return 0; }; STEP=probe; unsure "self-test" 2>/dev/null) && rc=0 || rc=$?
    check test "$rc/$(tail -n 1 <<<"$out")" = "2/rendezvous-check result=fail step=probe"
    out=$(release() { return 1; }; STEP=probe; unsure "self-test" 2>/dev/null) && rc=0 || rc=$?
    check test "$rc/$(tail -n 1 <<<"$out")" = "1/rendezvous-check result=fail step=cleanup"
    out=$(release() { return 0; }; STEP=probe; fail_at "self-test" 2>/dev/null) && rc=0 || rc=$?
    check test "$rc/$(tail -n 1 <<<"$out")" = "1/rendezvous-check result=fail step=probe"
    out=$(release() { echo "rendezvous-check released"; return 1; }; STEP=probe; fail_at "self-test" 2>/dev/null) && rc=0 || rc=$?
    check test "$rc/$(sed -n 1p <<<"$out")/$(tail -n 1 <<<"$out")" = "1/rendezvous-check released/rendezvous-check result=fail step=cleanup"
    if [[ "$fails" -eq 0 ]]; then
        log "자체 시험 통과 — 사례 ${n}개"
        echo "rendezvous-check container self-test result=ok"
        return 0
    fi
    echo "rendezvous-check container self-test result=fail failures=$fails"
    return 1
}

# ── 실제 검사 ────────────────────────────────────────────────────────

TMP=""
CONTAINER=""
VOLUME=""
BUILT_IMAGE=""
PEER_PIDS=()
# 이 실행의 이름 조각과 라벨 키. 조각은 main_check가 정한다.
TOKEN=""
LABEL=tasty.container-check

# /dev/urandom에서 얻은 16진수 12자.
new_token() { od -An -N6 -tx1 /dev/urandom | tr -d ' \n'; }

# exists <container|volume|image> <이름>: 그 이름이 있다.
exists() { docker "$1" inspect "$2" >/dev/null 2>&1; }

# owned <container|volume|image> <이름>: 그 이름이 있고 라벨이 이 실행의 조각이다.
owned() {
    local fmt="{{index .Config.Labels \"$LABEL\"}}"
    [[ "$1" == volume ]] && fmt="{{index .Labels \"$LABEL\"}}"
    [[ -n "$TOKEN" && "$(docker "$1" inspect -f "$fmt" "$2" 2>/dev/null)" == "$TOKEN" ]]
}

# remove_owned <container|volume|image> <이름>: 이 실행이 만든 것이면 지운다. 이 실행의 것이 아니면 지우지
# 않고 실패한다.
remove_owned() {
    local kind=$1 name=$2
    if ! owned "$kind" "$name"; then
        log "정리: $kind $name은 이 실행의 라벨이 없어 지우지 않는다"
        return 1
    fi
    case "$kind" in
        container) docker rm -f "$name" >/dev/null 2>&1 ;;
        volume) docker volume rm "$name" >/dev/null 2>&1 ;;
        image) docker image rm "$name" >/dev/null 2>&1 ;;
    esac || { log "정리: $kind $name을 지우지 못했다"; return 1; }
}

# 프로세스 $1이 없다(bash는 끝난 자식을 바로 거두므로 끝난 자식도 없는 것으로 보인다).
gone() { ! kill -0 "$1" 2>/dev/null; }

# stop_peer <pid>: SIGTERM을 보내고 CLEANUP_SECONDS 안에 끝나기를 기다린다. 남으면 SIGKILL로 끝낸다.
# 어느 쪽이든 거둔다. SIGKILL 뒤에도 남으면 실패한다.
stop_peer() {
    local pid=$1
    gone "$pid" && { wait "$pid" 2>/dev/null; return 0; }
    kill -s TERM "$pid" 2>/dev/null
    if ! wait_until "$CLEANUP_SECONDS" gone "$pid"; then
        log "정리: 예제(pid $pid)가 SIGTERM 뒤 ${CLEANUP_SECONDS}s 안에 끝나지 않아 SIGKILL로 끝낸다"
        kill -s KILL "$pid" 2>/dev/null
    fi
    wait "$pid" 2>/dev/null
    gone "$pid" || { log "정리: 예제(pid $pid)가 끝나지 않았다"; return 1; }
}

# release: 남은 예제를 끝내고 이 실행의 컨테이너·볼륨·이미지와 임시 폴더를 지운 뒤 남지 않았는지 본다.
# 다 되면 0, 하나라도 안 되면 1. 다시 불러도 된다(지운 것은 잊는다).
release() {
    local p bad=0
    for p in ${PEER_PIDS[@]+"${PEER_PIDS[@]}"}; do
        stop_peer "$p" || bad=1
    done
    PEER_PIDS=()
    if [[ -n "$CONTAINER" ]]; then
        if remove_owned container "$CONTAINER" && ! exists container "$CONTAINER"; then CONTAINER=""; else bad=1; fi
    fi
    if [[ -n "$VOLUME" ]]; then
        if remove_owned volume "$VOLUME" && ! exists volume "$VOLUME"; then VOLUME=""; else bad=1; fi
    fi
    if [[ -n "$BUILT_IMAGE" ]]; then
        if remove_owned image "$BUILT_IMAGE" && ! exists image "$BUILT_IMAGE"; then BUILT_IMAGE=""; else bad=1; fi
    fi
    if [[ -n "$TMP" ]]; then
        if rm -rf "${TMP:?}" && [[ ! -e "$TMP" ]]; then TMP=""; else bad=1; fi
    fi
    return "$bad"
}

# 실패 경로의 정리. 이미 실패로 끝나는 중이라 정리 실패는 로그로만 남긴다(종료 코드는 0이 아니다).
cleanup() { release || log "정리: 남은 것이 있다(위 줄)"; }
trap cleanup EXIT

STEP=""
fail_at() {
    log "$1"
    if [[ -n "$CONTAINER" ]]; then
        log "--- 서버 로그"
        docker logs "$CONTAINER" >&2 2>&1 || true
    fi
    local f
    for f in "${TMP:-/nonexistent}"/*.log; do
        [[ -f "$f" ]] || continue
        log "--- $(basename "$f")"
        cat "$f" >&2
    done
    finish 1
}
unsure() {
    log "판정 불가: $1"
    finish 2
}
# finish <rc>: 실패·판정 불가 경로의 끝. 성공 경로와 같은 규칙으로 정리(release)를 먼저 하고 결과 줄을 쓴다.
# 정리가 되면 result=fail step=<단계>와 rc, 정리가 실패하면 결과를 result=fail step=cleanup과 rc 1로 덮는다.
finish() {
    if release; then
        trap - EXIT
        echo "rendezvous-check result=fail step=$STEP"
        exit "$1"
    fi
    trap - EXIT
    echo "rendezvous-check result=fail step=cleanup"
    exit 1
}
step_ok() { echo "rendezvous-check step=$STEP ok${1:+ $1}"; }

# 기한 $1초 안에 명령이 성공할 때까지 POLL_SECONDS마다 다시 부른다. 성공하면 0, 기한을 넘기면 1.
wait_until() {
    local deadline=$((SECONDS + $1))
    shift
    until "$@"; do
        [[ "$SECONDS" -lt "$deadline" ]] || return 1
        sleep "$POLL_SECONDS"
    done
}

# 프로세스 $@가 모두 끝났다.
all_ended() {
    local p
    for p in "$@"; do
        kill -0 "$p" 2>/dev/null && return 1
    done
    return 0
}

# cargo build --message-format=json 출력 $1에서 target 이름 $2의 실행 파일 경로를 낸다.
executable_of() {
    grep -F '"reason":"compiler-artifact"' <<<"$1" \
        | grep -F "\"name\":\"$2\"" \
        | grep -o '"executable":"[^"]*"' \
        | sed 's/^"executable":"//; s/"$//' \
        | tail -n 1
}

# 127.0.0.1의 비어 있는 UDP 포트 둘. 두 소켓을 함께 열어 서로 다른 포트를 받는다.
free_udp_ports() {
    python3 -c '
import socket
a = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
b = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
a.bind(("127.0.0.1", 0))
b.bind(("127.0.0.1", 0))
print(a.getsockname()[1], b.getsockname()[1])
'
}

# 컨테이너 eth0의 수신 카운터 "바이트 패킷".
server_rx() {
    docker exec "$CONTAINER" cat /sys/class/net/eth0/statistics/rx_bytes /sys/class/net/eth0/statistics/rx_packets \
        | tr '\n' ' ' | sed 's/ $//'
}

# server_rx를 읽어 "바이트 패킷" 두 십진수일 때만 낸다. 아니면 읽은 것을 내고 실패한다.
rx_snapshot() {
    local v
    v=$(server_rx) || { echo "$v"; return 1; }
    [[ "$v" =~ ^[0-9]{1,18}\ [0-9]{1,18}$ ]] || { echo "$v"; return 1; }
    echo "$v"
}

# run_pair <이름> <메시지 바이트>: listener와 dialer를 띄워 메시지를 echo하고, 두 예제의 연결 정보 줄을
# dial_line·listen_line에 둔다. 출력은 TMP/<이름>-*.out·.log다. 실패하면 fail_at으로 끝난다.
# main_check의 peer·pub·primary_port·host_ip·listener_id·dialer_id를 쓴다.
run_pair() {
    local name=$1 bytes=$2 message listen_rc dial_rc
    message=$(printf '%*s' "$bytes" '' | tr ' ' x)
    local common=(--server "127.0.0.1:$primary_port" --server-key "$pub" --timeout "$PEER_TIMEOUT"
        --host-ip "$host_ip")
    "$peer" --listen --key-dir "$TMP/listener" --peer "$dialer_id" ${common[@]+"${common[@]}"} \
        >"$TMP/$name-listener.out" 2>"$TMP/$name-listener.log" &
    PEER_PIDS+=($!)
    "$peer" --dial "$listener_id" --key-dir "$TMP/dialer" --message "$message" ${common[@]+"${common[@]}"} \
        >"$TMP/$name-dialer.out" 2>"$TMP/$name-dialer.log" &
    PEER_PIDS+=($!)
    wait_until "$PEER_WAIT_SECONDS" all_ended ${PEER_PIDS[@]+"${PEER_PIDS[@]}"} \
        || fail_at "예제($name)가 ${PEER_WAIT_SECONDS}s 안에 끝나지 않았다"
    listen_rc=0
    wait "${PEER_PIDS[0]}" || listen_rc=$?
    dial_rc=0
    wait "${PEER_PIDS[1]}" || dial_rc=$?
    PEER_PIDS=()
    dial_line=$(grep '^echo ok ' "$TMP/$name-dialer.out" | head -n 1)
    listen_line=$(grep '^echoed ' "$TMP/$name-listener.out" | head -n 1)
    [[ "$dial_rc" -eq 0 && -n "$dial_line" ]] || fail_at "dialer($name) 종료 코드 $dial_rc, echo 확인 줄 없음"
    [[ "$listen_rc" -eq 0 && -n "$listen_line" ]] || fail_at "listener($name) 종료 코드 $listen_rc, echo 줄 없음"
    [[ "$listen_line" == "echoed $bytes bytes from $dialer_id via "* ]] \
        || fail_at "listener($name)가 echo한 크기나 상대가 다르다: $listen_line"
}

server_logs() { docker logs "$CONTAINER" 2>&1; }
server_started() { server_logs | grep -qF "rendezvous server started public_key=$1 "; }

main_check() {
    local tool json peer peer_from image image_from sha host_ip ports primary_port alternate_port cfg out keyout pub
    local listener_id dialer_id
    local dial_line listen_line reflexive nat mode why state rc

    STEP=build
    for tool in docker cargo python3 ip git; do
        command -v "$tool" >/dev/null 2>&1 || unsure "$tool이 PATH에 없다"
    done
    MESSAGE_BYTES=$(decimal_in "$MESSAGE_BYTES" 1 "$MESSAGE_BYTES_MAX") \
        || unsure "CONTAINER_CHECK_MESSAGE_BYTES는 1–$MESSAGE_BYTES_MAX의 십진수다: ${CONTAINER_CHECK_MESSAGE_BYTES:-}"
    START_SECONDS=$(decimal_in "$START_SECONDS" 1 "$SECONDS_MAX") \
        || unsure "CONTAINER_CHECK_START_SECONDS는 1–$SECONDS_MAX의 십진수다: ${CONTAINER_CHECK_START_SECONDS:-}"
    PEER_TIMEOUT=$(decimal_in "$PEER_TIMEOUT" 1 "$SECONDS_MAX") \
        || unsure "CONTAINER_CHECK_PEER_TIMEOUT은 1–$SECONDS_MAX의 십진수다: ${CONTAINER_CHECK_PEER_TIMEOUT:-}"
    STOP_SECONDS=$(decimal_in "$STOP_SECONDS" 1 "$SECONDS_MAX") \
        || unsure "CONTAINER_CHECK_STOP_SECONDS는 1–$SECONDS_MAX의 십진수다: ${CONTAINER_CHECK_STOP_SECONDS:-}"
    PEER_WAIT_SECONDS=$((PEER_TIMEOUT + PEER_WAIT_EXTRA))
    TMP=$(mktemp -d) || unsure "임시 폴더를 만들지 못했다"
    TOKEN=$(new_token)
    [[ "$TOKEN" =~ ^[0-9a-f]{12}$ ]] || unsure "/dev/urandom에서 이름 조각을 얻지 못했다: '$TOKEN'"
    sha=$(git -C "$ROOT" rev-parse HEAD 2>/dev/null) || sha=""
    [[ "$sha" =~ ^[0-9a-f]{40}$ ]] || unsure "실행 sha(git HEAD)를 읽지 못했다: '$sha'"
    if [[ -n "${P2P_PEER:-}" ]]; then
        [[ -x "$P2P_PEER" ]] || unsure "P2P_PEER가 실행 파일이 아니다: $P2P_PEER"
        peer=$P2P_PEER
        peer_from=given
    else
        peer_from=built
        log "빌드: ${PEER_BUILD_CMD[*]}"
        json=$(cd "$ROOT" && ${PEER_BUILD_CMD[@]+"${PEER_BUILD_CMD[@]}"}) || unsure "p2p_peer 빌드 실패"
        # 실행 파일 경로는 cargo 출력의 executable 값에서 얻는다(target 폴더 위치를 짐작하지 않는다).
        peer=$(executable_of "$json" p2p_peer)
        [[ -x "$peer" ]] || unsure "빌드 출력에서 p2p_peer 실행 파일을 찾지 못했다: $peer"
    fi
    if [[ -n "${CONTAINER_CHECK_IMAGE:-}" ]]; then
        image=$CONTAINER_CHECK_IMAGE
        docker image inspect "$image" >/dev/null 2>&1 || unsure "이미지가 없다: $image"
        image_from=given
    else
        image_from=built
        image="tasty-rendezvous:container-check-$TOKEN"
        exists image "$image" && unsure "같은 이름의 이미지가 이미 있다: $image"
        log "빌드: docker build -f rendezvous/Dockerfile --label $LABEL=$TOKEN -t $image ."
        docker build -f "$HERE/Dockerfile" --label "$LABEL=$TOKEN" -t "$image" "$ROOT" \
            >"$TMP/image-build.log" 2>&1 || {
            cat "$TMP/image-build.log" >&2
            unsure "이미지 빌드 실패"
        }
        BUILT_IMAGE=$image
    fi
    step_ok "sha=$sha image=$image_from peer=$peer_from image_id=$(docker image inspect -f '{{.Id}}' "$image")"

    STEP=config
    if [[ -n "${CONTAINER_CHECK_HOST_IP:-}" ]]; then
        host_ip=$CONTAINER_CHECK_HOST_IP
    else
        host_ip=$(ip -4 route get "$ROUTE_PROBE_IP" 2>/dev/null | sed -n 's/.* src \([0-9.]*\).*/\1/p' | head -n 1)
    fi
    is_private_ipv4 "$host_ip" \
        || unsure "기기가 알릴 사설 IPv4가 없다(얻은 값: '${host_ip}'). CONTAINER_CHECK_HOST_IP로 준다"
    ports=$(free_udp_ports) || unsure "빈 UDP 포트를 고르지 못했다"
    read -r primary_port alternate_port <<<"$ports"
    cfg=$TMP/rendezvous.toml
    advertise_config "$EXAMPLE" 127.0.0.1 "$alternate_port" >"$cfg" \
        || fail_at "예시의 [advertise] primary_ip·alternate_port 줄을 바꾸지 못했다"
    # 컨테이너 사용자(UID 10001)가 읽는다.
    chmod 0644 "$cfg"
    out=$(docker run --rm -v "$cfg:/etc/tasty-rendezvous/rendezvous.toml:ro" "$image" \
        settings --config /etc/tasty-rendezvous/rendezvous.toml 2>&1) || fail_at "settings --config 실패: $out"
    grep -qx 'advertise.primary_ip = 127.0.0.1 (configured)' <<<"$out" \
        || fail_at "advertise.primary_ip 줄: $(grep '^advertise.primary_ip' <<<"$out")"
    grep -qx "advertise.alternate_port = $alternate_port (configured)" <<<"$out" \
        || fail_at "advertise.alternate_port 줄: $(grep '^advertise.alternate_port' <<<"$out")"
    step_ok "advertise=127.0.0.1:$alternate_port published=127.0.0.1:$primary_port,127.0.0.1:$alternate_port host_ip=private-ipv4"

    STEP=keygen
    local volume="tasty-rendezvous-container-check-$TOKEN"
    # docker volume create는 같은 이름의 볼륨이 있으면 그것을 돌려주므로 먼저 없는지 본다.
    exists volume "$volume" && fail_at "같은 이름의 볼륨이 이미 있다: $volume"
    docker volume create --label "$LABEL=$TOKEN" "$volume" >/dev/null || fail_at "볼륨을 만들지 못했다"
    VOLUME=$volume
    keyout=$(docker run --rm -v "$VOLUME:/var/lib/tasty-rendezvous" "$image" \
        keygen --dir /var/lib/tasty-rendezvous 2>&1) || fail_at "keygen 실패: $keyout"
    pub=$(sed -n 's/^public key: \([0-9a-f]\{64\}\)$/\1/p' <<<"$keyout")
    [[ -n "$pub" ]] || fail_at "keygen 출력에 공개키가 없다: $keyout"
    step_ok "public_key=$pub"

    STEP=start
    local container="tasty-rendezvous-container-check-$TOKEN"
    exists container "$container" && fail_at "같은 이름의 컨테이너가 이미 있다: $container"
    # debug 로그는 서버가 본 기기의 반사 주소와 후보를 찍는다(path 단계가 읽는다).
    docker run -d --name "$container" --label "$LABEL=$TOKEN" \
        -p "127.0.0.1:$primary_port:$CONTAINER_PRIMARY_PORT/udp" \
        -p "127.0.0.1:$alternate_port:$CONTAINER_ALTERNATE_PORT/udp" \
        -v "$VOLUME:/var/lib/tasty-rendezvous" \
        -v "$cfg:/etc/tasty-rendezvous/rendezvous.toml:ro" \
        -e TASTY_RENDEZVOUS_LOG=debug \
        "$image" >"$TMP/run.log" 2>&1 || {
        local r
        r=$(cat "$TMP/run.log")
        # 만든 뒤 시작에 실패한 컨테이너는 이 실행의 라벨을 달고 있다. 그때만 정리 대상으로 둔다.
        owned container "$container" && CONTAINER=$container
        fail_at "컨테이너를 띄우지 못했다: $r"
    }
    CONTAINER=$container
    rm -f "$TMP/run.log"
    wait_until "$START_SECONDS" server_started "$pub" || fail_at "서버 시작 줄이 ${START_SECONDS}s 안에 나오지 않았다"
    step_ok "container=$CONTAINER"

    listener_id=$("$peer" id --key-dir "$TMP/listener" 2>&1) || fail_at "listener 키: $listener_id"
    dialer_id=$("$peer" id --key-dir "$TMP/dialer" 2>&1) || fail_at "dialer 키: $dialer_id"
    nat=""
    mode=""

    # 대조 실행(1바이트)과 데이터 실행(MESSAGE_BYTES)을 같은 서버로 차례로 돌리고, 실행마다 앞뒤의 서버
    # 쪽 수신 카운터를 읽는다.
    local run bytes log_start run_log before after rx0 rx1 pk0 pk1 control_bytes control_packets data_bytes data_packets
    for run in control data; do
        bytes=1
        [[ "$run" == data ]] && bytes=$MESSAGE_BYTES
        STEP=connect-$run
        log_start=$(server_logs | wc -l | tr -d ' ')
        before=$(rx_snapshot) || fail_at "서버 쪽 수신 카운터를 읽지 못했다: '$before'"
        run_pair "$run" "$bytes"
        after=$(rx_snapshot) || fail_at "서버 쪽 수신 카운터를 읽지 못했다: '$after'"
        read -r rx0 pk0 <<<"$before"
        read -r rx1 pk1 <<<"$after"
        step_ok "dialer: $dial_line"
        step_ok "listener: $listen_line"
        step_ok "server_rx_bytes_delta=$((rx1 - rx0)) server_rx_packets_delta=$((pk1 - pk0))"

        STEP=path-$run
        run_log=$(log_after "$(server_logs)" "$log_start") || fail_at "서버 로그 줄 수를 읽지 못했다: '$log_start'"
        reflexive=$(reflexive_of "$run_log")
        nat=$(grep -F 'nat report' <<<"$run_log" | sed -n 's/.* mapping=\([^ ]*\).*/\1/p' | sort -u | tr '\n' ',' | sed 's/,$//')
        mode=$(grep -F 'punch offered' <<<"$run_log" | sed -n 's/.* mode=\([^ ]*\).*/\1/p' | tail -n 1)
        why=$(path_ok "$dial_line" "$listen_line" "$host_ip" "$reflexive") || fail_at "$why (nat=$nat mode=$mode)"
        step_ok "path=host devices=same-host-processes network=private-ipv4 nat=$nat mode=$mode"

        if [[ "$run" == control ]]; then
            control_bytes=$((rx1 - rx0))
            control_packets=$((pk1 - pk0))
        else
            data_bytes=$((rx1 - rx0))
            data_packets=$((pk1 - pk0))
        fi
    done

    STEP=no-forward
    local judged=0
    why=$(no_forward_ok "$data_bytes" "$control_bytes" "$MESSAGE_BYTES" "$data_packets" "$control_packets") || judged=$?
    case "$judged" in
        0) ;;
        2) unsure "$why" ;;
        *) fail_at "$why" ;;
    esac
    step_ok "control_rx_bytes=$control_bytes control_rx_packets=$control_packets data_rx_bytes=$data_bytes data_rx_packets=$data_packets bound_bytes=$((2 * control_bytes)) message_bytes=$MESSAGE_BYTES device_bytes_at_least=$((2 * MESSAGE_BYTES))"

    STEP=stop
    docker stop --time "$STOP_SECONDS" "$CONTAINER" >/dev/null 2>&1 || fail_at "docker stop 실패"
    state=$(docker inspect -f '{{.State.ExitCode}} {{.State.OOMKilled}}' "$CONTAINER") || fail_at "컨테이너 상태를 읽지 못했다"
    rc=${state%% *}
    # docker stop은 기한을 넘기면 SIGKILL로 끝낸다(종료 코드 137).
    [[ "$rc" == 0 ]] || fail_at "서버 종료 코드 $rc(SIGINT 뒤 ${STOP_SECONDS}s)"
    server_logs | grep -q 'shutting down' || fail_at "종료 로그가 없다"
    step_ok "rc=$rc"

    STEP=cleanup
    release || { echo "rendezvous-check result=fail step=$STEP"; exit 1; }
    step_ok

    echo "rendezvous-check result=ok devices=same-host-processes path=host image=$image_from peer=$peer_from sha=$sha"
}

case "${1:-}" in
    "") main_check ;;
    --self-test) self_test ;;
    -h | --help) show_help ;;
    *)
        show_help >&2
        exit 2
        ;;
esac
