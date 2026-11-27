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
#      주면 그 이미지). 실행 sha를 적는다.
#   2. config: 예시 설정의 광고 두 줄을 바꾼 사본을 이미지의 settings --config로 읽어 두 값이 configured인지 본다.
#   3. keygen: 임시 볼륨에 서버 키를 만들고 공개키를 얻는다.
#   4. start: 컨테이너를 띄워 시작 줄(같은 공개키)을 기다리고, 서버 쪽 수신 카운터(컨테이너 eth0의
#      rx_bytes·rx_packets)를 읽는다.
#   5. connect: 두 기기가 서버의 소개로 붙어 CONTAINER_CHECK_MESSAGE_BYTES 바이트 메시지를 echo한다.
#      두 예제의 연결 정보 줄(경로·주소쌍·서버)을 그대로 적는다.
#   6. path: 두 기기의 성공 후보 종류가 host이고 상대 주소가 알린 호스트 IP인지, 서버가 본 반사 주소와
#      다른지 본다. 서버 로그의 매핑 판별과 펀칭 mode도 적는다.
#   7. no-forward: 서버 쪽 수신 바이트의 증가가 메시지 크기보다 작은지 본다. 기기 사이에는 메시지와
#      그 echo로 메시지 크기의 두 배 이상이 오갔으므로, 그 데이터가 서버를 지났다면 증가가 이보다 크다.
#   8. stop: docker stop(SIGINT)으로 멈추고 종료 코드 0과 종료 로그를 본다.
#   9. 결과를 rendezvous-check step=<단계> ok 줄들과 마지막 줄 rendezvous-check result=ok|fail로 쓴다.
#      result=ok 줄에 기기 구성(devices=same-host-processes), 경로, 실행 sha를 함께 적는다.
# 컨테이너·볼륨·임시 폴더와 이 검사가 빌드한 이미지는 끝날 때 지운다.
#
# 필요한 것: Linux Docker Engine, cargo, python3(빈 포트 고르기), ip(사설 IPv4 찾기).
# 종료 코드: 0 통과, 1 실패, 2 판정 불가(도구 없음, 빌드 실패, 사설 IPv4 없음, 인자 오류).
#
# 환경 변수:
#   P2P_PEER                          p2p_peer 예제 실행 파일. 주면 빌드하지 않는다
#   CONTAINER_CHECK_IMAGE             서버 이미지. 주면 빌드하지 않고 지우지도 않는다
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

# 서버 쪽 수신 바이트 증가 $1이 메시지 크기 $2보다 작다. 기기 사이 데이터(메시지와 echo, $2의 두 배 이상)가
# 서버를 지났다면 증가가 $2보다 크다.
no_forward_ok() {
    [[ "$1" =~ ^[0-9]+$ && "$2" =~ ^[0-9]+$ ]] || return 1
    (($1 < $2))
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
    check no_forward_ok 5000 98304
    check not no_forward_ok 98304 98304
    check not no_forward_ok 200000 98304
    check not no_forward_ok '' 98304
    check test "$(decimal_in 08 1 120000)" = 8
    check test "$(decimal_in 98304 1 120000)" = 98304
    check not decimal_in 0 1 120000
    check not decimal_in 120001 1 120000
    check not decimal_in 18446744073709649920 1 120000
    check not decimal_in -5 1 120000
    check not decimal_in '' 1 120000
    check not decimal_in 1e3 1 120000
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
cleanup() {
    local p
    for p in ${PEER_PIDS[@]+"${PEER_PIDS[@]}"}; do
        kill "$p" 2>/dev/null || true
    done
    if [[ -n "$CONTAINER" ]]; then
        docker rm -f "$CONTAINER" >/dev/null 2>&1 || log "정리: 컨테이너 $CONTAINER를 지우지 못했다"
    fi
    if [[ -n "$VOLUME" ]]; then
        docker volume rm "$VOLUME" >/dev/null 2>&1 || log "정리: 볼륨 $VOLUME을 지우지 못했다"
    fi
    if [[ -n "$BUILT_IMAGE" ]]; then
        docker image rm "$BUILT_IMAGE" >/dev/null 2>&1 || log "정리: 이미지 $BUILT_IMAGE를 지우지 못했다"
    fi
    if [[ -n "$TMP" ]]; then
        rm -rf "${TMP:?}"
    fi
}
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
    echo "rendezvous-check result=fail step=$STEP"
    exit 1
}
unsure() {
    log "판정 불가: $1"
    echo "rendezvous-check result=fail step=$STEP"
    exit 2
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

server_logs() { docker logs "$CONTAINER" 2>&1; }
server_started() { server_logs | grep -qF "rendezvous server started public_key=$1 "; }

main_check() {
    local tool json peer image sha host_ip ports primary_port alternate_port cfg out keyout pub
    local before after rx0 rx1 pk0 pk1 message listener_id dialer_id dial_rc listen_rc
    local dial_line listen_line dial_path listen_path dial_remote listen_remote reflexive nat mode state rc

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
    sha=$(git -C "$ROOT" rev-parse HEAD 2>/dev/null) || sha=unknown
    if [[ -n "${P2P_PEER:-}" ]]; then
        [[ -x "$P2P_PEER" ]] || unsure "P2P_PEER가 실행 파일이 아니다: $P2P_PEER"
        peer=$P2P_PEER
    else
        log "빌드: ${PEER_BUILD_CMD[*]}"
        json=$(cd "$ROOT" && ${PEER_BUILD_CMD[@]+"${PEER_BUILD_CMD[@]}"}) || unsure "p2p_peer 빌드 실패"
        # 실행 파일 경로는 cargo 출력의 executable 값에서 얻는다(target 폴더 위치를 짐작하지 않는다).
        peer=$(executable_of "$json" p2p_peer)
        [[ -x "$peer" ]] || unsure "빌드 출력에서 p2p_peer 실행 파일을 찾지 못했다: $peer"
    fi
    if [[ -n "${CONTAINER_CHECK_IMAGE:-}" ]]; then
        image=$CONTAINER_CHECK_IMAGE
        docker image inspect "$image" >/dev/null 2>&1 || unsure "이미지가 없다: $image"
    else
        image="tasty-rendezvous:container-check-$$"
        log "빌드: docker build -f rendezvous/Dockerfile -t $image ."
        docker build -f "$HERE/Dockerfile" -t "$image" "$ROOT" >"$TMP/image-build.log" 2>&1 || {
            cat "$TMP/image-build.log" >&2
            unsure "이미지 빌드 실패"
        }
        BUILT_IMAGE=$image
    fi
    step_ok "sha=$sha image=$(docker image inspect -f '{{.Id}}' "$image")"

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
    VOLUME="tasty-rendezvous-container-check-$$"
    docker volume create "$VOLUME" >/dev/null || fail_at "볼륨을 만들지 못했다"
    keyout=$(docker run --rm -v "$VOLUME:/var/lib/tasty-rendezvous" "$image" \
        keygen --dir /var/lib/tasty-rendezvous 2>&1) || fail_at "keygen 실패: $keyout"
    pub=$(sed -n 's/^public key: \([0-9a-f]\{64\}\)$/\1/p' <<<"$keyout")
    [[ -n "$pub" ]] || fail_at "keygen 출력에 공개키가 없다: $keyout"
    step_ok "public_key=$pub"

    STEP=start
    CONTAINER="tasty-rendezvous-container-check-$$"
    # debug 로그는 서버가 본 기기의 반사 주소와 후보를 찍는다(path 단계가 읽는다).
    docker run -d --name "$CONTAINER" \
        -p "127.0.0.1:$primary_port:$CONTAINER_PRIMARY_PORT/udp" \
        -p "127.0.0.1:$alternate_port:$CONTAINER_ALTERNATE_PORT/udp" \
        -v "$VOLUME:/var/lib/tasty-rendezvous" \
        -v "$cfg:/etc/tasty-rendezvous/rendezvous.toml:ro" \
        -e TASTY_RENDEZVOUS_LOG=debug \
        "$image" >"$TMP/run.log" 2>&1 || {
        local r
        r=$(cat "$TMP/run.log")
        CONTAINER=""
        docker rm -f "tasty-rendezvous-container-check-$$" >/dev/null 2>&1 || true
        fail_at "컨테이너를 띄우지 못했다: $r"
    }
    rm -f "$TMP/run.log"
    wait_until "$START_SECONDS" server_started "$pub" || fail_at "서버 시작 줄이 ${START_SECONDS}s 안에 나오지 않았다"
    before=$(server_rx) || fail_at "서버 쪽 수신 카운터를 읽지 못했다"
    read -r rx0 pk0 <<<"$before"
    step_ok "container=$CONTAINER server_rx_bytes=$rx0 server_rx_packets=$pk0"

    STEP=connect
    listener_id=$("$peer" id --key-dir "$TMP/listener" 2>&1) || fail_at "listener 키: $listener_id"
    dialer_id=$("$peer" id --key-dir "$TMP/dialer" 2>&1) || fail_at "dialer 키: $dialer_id"
    message=$(printf '%*s' "$MESSAGE_BYTES" '' | tr ' ' x)
    local common=(--server "127.0.0.1:$primary_port" --server-key "$pub" --timeout "$PEER_TIMEOUT"
        --host-ip "$host_ip")
    "$peer" --listen --key-dir "$TMP/listener" --peer "$dialer_id" ${common[@]+"${common[@]}"} \
        >"$TMP/listener.out" 2>"$TMP/listener.log" &
    PEER_PIDS+=($!)
    "$peer" --dial "$listener_id" --key-dir "$TMP/dialer" --message "$message" ${common[@]+"${common[@]}"} \
        >"$TMP/dialer.out" 2>"$TMP/dialer.log" &
    PEER_PIDS+=($!)
    wait_until "$PEER_WAIT_SECONDS" all_ended ${PEER_PIDS[@]+"${PEER_PIDS[@]}"} \
        || fail_at "예제가 ${PEER_WAIT_SECONDS}s 안에 끝나지 않았다"
    listen_rc=0
    wait "${PEER_PIDS[0]}" || listen_rc=$?
    dial_rc=0
    wait "${PEER_PIDS[1]}" || dial_rc=$?
    PEER_PIDS=()
    dial_line=$(grep '^echo ok ' "$TMP/dialer.out" | head -n 1)
    listen_line=$(grep '^echoed ' "$TMP/listener.out" | head -n 1)
    [[ "$dial_rc" -eq 0 && -n "$dial_line" ]] || fail_at "dialer 종료 코드 $dial_rc, echo 확인 줄 없음"
    [[ "$listen_rc" -eq 0 && -n "$listen_line" ]] || fail_at "listener 종료 코드 $listen_rc, echo 줄 없음"
    [[ "$listen_line" == "echoed $MESSAGE_BYTES bytes from $dialer_id via "* ]] \
        || fail_at "listener가 echo한 크기나 상대가 다르다: $listen_line"
    step_ok "dialer: $dial_line"
    step_ok "listener: $listen_line"

    STEP=path
    dial_path=$(field_of "$dial_line" path)
    listen_path=$(field_of "$listen_line" path)
    dial_remote=$(field_of "$dial_line" remote)
    listen_remote=$(field_of "$listen_line" remote)
    reflexive=$(server_logs | grep -F 'registered addresses' | sed -n 's/.* reflexive=\([^ ]*\).*/\1/p' | sort -u | tr '\n' ',' | sed 's/,$//')
    nat=$(server_logs | grep -F 'nat report' | sed -n 's/.* mapping=\([^ ]*\).*/\1/p' | sort -u | tr '\n' ',' | sed 's/,$//')
    mode=$(server_logs | grep -F 'punch offered' | sed -n 's/.* mode=\([^ ]*\).*/\1/p' | tail -n 1)
    if [[ "$dial_path" != host || "$listen_path" != host ]]; then
        case "$dial_path/$listen_path" in
            *reflexive*) fail_at "반사 후보로 붙었다(dialer path=$dial_path, listener path=$listen_path, 서버가 본 반사 주소 $reflexive, mode=$mode). 게시 포트를 docker-proxy가 나르지 않는 Docker로 보인다" ;;
            *) fail_at "성공 후보 종류가 host가 아니다: dialer path=$dial_path, listener path=$listen_path" ;;
        esac
    fi
    [[ "$(ip_of "$dial_remote")" == "$host_ip" && "$(ip_of "$listen_remote")" == "$host_ip" ]] \
        || fail_at "상대 주소가 알린 호스트 IP가 아니다: dialer remote=$dial_remote, listener remote=$listen_remote"
    [[ -n "$reflexive" ]] || fail_at "서버 로그에 기기의 반사 주소(registered addresses)가 없다"
    case ",$reflexive," in
        *",$dial_remote,"* | *",$listen_remote,"*) fail_at "연결 주소가 서버가 본 반사 주소와 같다: $reflexive" ;;
    esac
    step_ok "path=host devices=same-host-processes network=private-ipv4 nat=$nat mode=$mode"

    STEP=no-forward
    after=$(server_rx) || fail_at "서버 쪽 수신 카운터를 읽지 못했다"
    read -r rx1 pk1 <<<"$after"
    no_forward_ok "$((rx1 - rx0))" "$MESSAGE_BYTES" \
        || fail_at "서버 쪽 수신 바이트가 메시지 크기 이상 늘었다: $((rx1 - rx0)) >= $MESSAGE_BYTES"
    step_ok "server_rx_bytes_delta=$((rx1 - rx0)) server_rx_packets_delta=$((pk1 - pk0)) message_bytes=$MESSAGE_BYTES device_bytes_at_least=$((2 * MESSAGE_BYTES))"

    STEP=stop
    docker stop --time "$STOP_SECONDS" "$CONTAINER" >/dev/null 2>&1 || fail_at "docker stop 실패"
    state=$(docker inspect -f '{{.State.ExitCode}} {{.State.OOMKilled}}' "$CONTAINER") || fail_at "컨테이너 상태를 읽지 못했다"
    rc=${state%% *}
    # docker stop은 기한을 넘기면 SIGKILL로 끝낸다(종료 코드 137).
    [[ "$rc" == 0 ]] || fail_at "서버 종료 코드 $rc(SIGINT 뒤 ${STOP_SECONDS}s)"
    server_logs | grep -q 'shutting down' || fail_at "종료 로그가 없다"
    step_ok "rc=$rc"

    echo "rendezvous-check result=ok devices=same-host-processes path=host sha=$sha"
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
