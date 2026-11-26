#!/usr/bin/env bash
# 실제 relay 프로세스(컨테이너)를 거친 P2P 연결 확인. 동작 수준 기준 D5 (b)다. CI가 아니라 원격 빌드 환경에서 돌린다.
#
#   relay/container-check.sh --peer-bin <p2p-peer 경로> [--keep-image] [설정.toml ...]
#   relay/container-check.sh --relay-only [--keep-image] [설정.toml ...]
#   relay/container-check.sh --self-test   docker·p2p-peer 없이 경로 판정과 peer 정리만 시험한다
#
# 설정마다 relay/check-config.sh와 같은 CI용 설정(TLS·QUIC 주소 확인 제외)으로 고정 이미지를 띄우고
# 같은 기동 판정(HTTP·metrics·수신 상한)을 한 뒤, p2p-peer 두 개를 그 relay에 붙인다.
# p2p-peer는 relay 경로가 선택된 뒤 직접 경로로 바뀌어야 통과한다. 가정한 p2p-peer 옵션, 경로 줄 형식,
# 통과 조건은 아래 cc_run_peers 주석에 있다(아직 p2p-peer가 없다).
# 결과 줄:
#   relay-check config=<이름> result=ok ...           기동 판정과 p2p-peer 연결이 모두 통과
#   relay-check config=<이름> result=relay-only ...   --relay-only: 기동 판정만 통과, p2p-peer는 돌리지 않음(D5 (b) 미충족)
#   relay-check config=<이름> result=fail ...         어느 단계든 실패
# p2p-peer 로그의 경로 바뀜 줄(path-selected)은 "relay-check config=<이름> peer-log: ..."로 옮긴다.
# 띄운 컨테이너·프로세스·임시 폴더는 끝날 때 지운다. 이 스크립트가 받은 이미지도 지운다(--keep-image면 남긴다).
#
# 종료 코드: 0 모든 설정 통과(--relay-only면 기동 판정만), 1 하나라도 실패, 2 판정 불가(docker·p2p-peer 없음, 인자 오류).
#
# 환경 변수: relay/check-config.sh의 것과 같고, 더해서
#   PEER_TIMEOUT_SECONDS  dialer가 끝나기를 기다리는 시간. 기본 60. 넘으면 TERM, 그 뒤 PEER_KILL_SECONDS에 KILL
#   PEER_KILL_SECONDS     TERM 뒤 KILL까지 기다리는 시간. 기본 5
set -uo pipefail

# shellcheck source=relay/check-config.sh
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/check-config.sh"

PEER_TIMEOUT_SECONDS="${PEER_TIMEOUT_SECONDS:-60}"
# TERM 뒤 KILL까지 기다리는 시간(listener 정리, dialer의 timeout -k).
PEER_KILL_SECONDS="${PEER_KILL_SECONDS:-5}"
CC_PEER_PIDS=()

cc_cleanup() {
    local p
    for p in "${CC_PEER_PIDS[@]}"; do
        cc_stop_peer "$p" || true
    done
    rc_cleanup
}

# p2p-peer 두 개를 relay URL $2에 붙인다. 인자: 이름, relay URL, p2p-peer 경로, metrics 주소.
#
# p2p-peer는 아직 없다(crates/tasty-net-demo, P2P 작업이 만든다). 아래는 이 스크립트가 가정한 인터페이스다.
# docs/design/relay.md "배포 형태"에도 같은 내용이 있다. p2p-peer가 생기면 그 실제 옵션·출력에 맞춰
# 이 함수와 cc_path_switched만 고친다.
#   listener: p2p-peer --listen --relay-url <URL> --key-dir <폴더> --addr-file <파일>
#             자기 주소(dialer가 --dial에 그대로 넘길 문자열)를 <파일>에 쓰고 연결 하나를 받는다.
#   dialer:   p2p-peer --dial <주소> --relay-url <URL> --key-dir <폴더>
#             선택 경로가 바뀔 때마다 한 줄을 쓴다:
#               p2p-peer path-selected path=relay relay=<relay URL>
#               p2p-peer path-selected path=direct addr=<IP:포트>
#             첫 선택이 relay 경로이고 그 뒤 직접 경로로 바뀌면 0으로 끝난다.
# 통과 조건(셋 다):
#   1. dialer 종료 코드 0.
#   2. dialer 로그의 첫 path-selected 줄이 이 relay(<URL>, 끝 / 무시)의 path=relay이고, 그 뒤에 path=direct 줄이 있다.
#      첫 선택이 직접인 연결(listener 주소에 직접 주소가 들어 있는 경우 등)은 나중에 relay를 거쳐도 통과하지 않는다.
#   3. peer 실행 전후로 relay metrics relayserver_accepts_total이 2 이상 늘었다(두 peer가 이 relay에 붙음).
cc_run_peers() {
    local name="$1" url="$2" peer="$3" metrics="$4" dir pid waited rc lines a0 a1
    dir="$RC_TMP/$name/peers"
    mkdir -p "$dir/a" "$dir/b" || return 1
    a0=$(cc_accepts "$metrics") || { RC_LAST_FAIL="stage=peers reason=metrics-before"; return 1; }
    "$peer" --listen --relay-url "$url" --key-dir "$dir/a" --addr-file "$dir/addr" > "$dir/listen.log" 2>&1 &
    pid=$!
    CC_PEER_PIDS+=("$pid")
    for ((waited = 0; waited < 30; waited++)); do
        [[ -s "$dir/addr" ]] && break
        cc_alive "$pid" || break
        sleep 1
    done
    if [[ ! -s "$dir/addr" ]]; then
        RC_LAST_FAIL="stage=peers reason=no-listener-addr"
        rc_mask < "$dir/listen.log" | sed 's/^/    /' >&2
        cc_stop_peer "$pid"
        return 1
    fi
    timeout -k "$PEER_KILL_SECONDS" "$PEER_TIMEOUT_SECONDS" \
        "$peer" --dial "$(cat "$dir/addr")" --relay-url "$url" --key-dir "$dir/b" > "$dir/dial.log" 2>&1
    rc=$?
    a1=$(cc_accepts "$metrics") || a1=""
    cc_stop_peer "$pid" || rc=1
    lines=$(grep -h 'path-selected' "$dir/dial.log" "$dir/listen.log" || true)
    if [[ -n "$lines" ]]; then
        while IFS= read -r l; do
            echo "relay-check config=$name peer-log: $l"
        done <<<"$lines"
    fi
    if [[ $rc != 0 ]]; then
        RC_LAST_FAIL="stage=peers dial_rc=$rc"
    elif ! cc_path_switched "$dir/dial.log" "$url"; then
        RC_LAST_FAIL="stage=peers reason=no-relay-then-direct"
    elif [[ -z "$a1" ]] || (( a1 - a0 < 2 )); then
        RC_LAST_FAIL="stage=peers reason=relay-accepts-${a0}-to-${a1:-unknown}"
    else
        return 0
    fi
    rc_log "dialer 로그(끝 40줄, 따옴표 문자열은 가림):"
    tail -40 "$dir/dial.log" | rc_mask | sed 's/^/    /' >&2
    return 1
}

# relay metrics 주소 $1의 relayserver_accepts_total 값.
cc_accepts() {
    local body v
    body=$(curl -fsS -m 5 "http://$1/metrics" 2>/dev/null) || return 1
    v=$(sed -n 's/^relayserver_accepts_total \([0-9][0-9]*\)$/\1/p' <<<"$body" | head -1)
    [[ -n "$v" ]] || return 1
    echo "$v"
}

# 로그 $1의 첫 path-selected 줄이 relay URL $2의 path=relay이고 그 뒤에 path=direct 줄이 있으면 0.
cc_path_switched() {
    awk -v want="${2%/}" '
        $0 ~ /(^|[[:space:]])path-selected([[:space:]]|$)/ {
            path = ""; relay = ""
            for (i = 1; i <= NF; i++) {
                if ($i ~ /^path=/) path = substr($i, 6)
                if ($i ~ /^relay=/) { relay = substr($i, 7); sub(/\/$/, "", relay) }
            }
            n++
            if (n == 1) { if (path == "relay" && relay == want) seen = 1; else exit }
            else if (path == "direct" && seen) { ok = 1; exit }
        }
        END { exit ok ? 0 : 1 }
    ' "$1"
}

# 프로세스 $1이 살아 있으면 0(회수 전 좀비는 끝난 것으로 본다).
cc_alive() {
    local st
    st=$(ps -o stat= -p "$1" 2>/dev/null) || return 1
    [[ -n "$st" && "$st" != Z* ]]
}

# PID로 끝낸다: TERM → PEER_KILL_SECONDS 동안 기다림 → KILL → 회수. 남으면 1.
cc_stop_peer() {
    local pid="$1" i t
    kill -TERM "$pid" 2>/dev/null || true
    for ((t = 0; t < PEER_KILL_SECONDS * 10; t++)); do
        cc_alive "$pid" || break
        sleep 0.1
    done
    if cc_alive "$pid"; then
        rc_log "p2p-peer가 TERM 뒤 ${PEER_KILL_SECONDS}초 안에 끝나지 않아 KILL한다: pid $pid"
        kill -KILL "$pid" 2>/dev/null || true
    fi
    wait "$pid" 2>/dev/null || true
    if cc_alive "$pid"; then
        rc_log "p2p-peer가 남았다: pid $pid"
        return 1
    fi
    for i in "${!CC_PEER_PIDS[@]}"; do
        [[ "${CC_PEER_PIDS[$i]}" == "$pid" ]] && unset 'CC_PEER_PIDS[i]'
    done
    return 0
}

# docker·p2p-peer 없이 경로 판정과 peer 정리를 시험한다. 통과 0, 실패 1.
cc_self_test() {
    local t fail=0 pid start i
    t=$(mktemp -d) || return 2
    st_case() { # 이름, 기대 rc, 실제 rc
        if [[ "$2" == "$3" ]]; then echo "self-test $1 ok"; else echo "self-test $1 FAIL want=$2 got=$3"; fail=1; fi
    }
    printf '%s\n' 'p2p-peer path-selected path=relay relay=http://127.0.0.1:5/' \
        'p2p-peer path-selected path=direct addr=127.0.0.1:9' > "$t/ok.log"
    cc_path_switched "$t/ok.log" "http://127.0.0.1:5"
    st_case relay-then-direct 0 $?
    printf '%s\n' 'p2p-peer path-selected path=direct addr=127.0.0.1:9' > "$t/direct.log"
    cc_path_switched "$t/direct.log" "http://127.0.0.1:5"
    st_case direct-only-rejected 1 $?
    printf '%s\n' 'p2p-peer path-selected path=direct addr=127.0.0.1:9' \
        'p2p-peer path-selected path=relay relay=http://127.0.0.1:5/' > "$t/rev.log"
    cc_path_switched "$t/rev.log" "http://127.0.0.1:5"
    st_case direct-then-relay-rejected 1 $?
    printf '%s\n' 'p2p-peer path-selected path=direct addr=127.0.0.1:9' \
        'p2p-peer path-selected path=relay relay=http://127.0.0.1:5/' \
        'p2p-peer path-selected path=direct addr=127.0.0.1:9' > "$t/drd.log"
    cc_path_switched "$t/drd.log" "http://127.0.0.1:5"
    st_case direct-relay-direct-rejected 1 $?
    printf '%s\n' 'p2p-peer path-selected path=relay relay=http://other:5/' \
        'p2p-peer path-selected path=relay relay=http://127.0.0.1:5/' \
        'p2p-peer path-selected path=direct addr=127.0.0.1:9' > "$t/orrd.log"
    cc_path_switched "$t/orrd.log" "http://127.0.0.1:5"
    st_case first-other-relay-rejected 1 $?
    printf '%s\n' 'p2p-peer path-selected path=relay relay=http://other:5/' \
        'p2p-peer path-selected path=direct addr=127.0.0.1:9' > "$t/other.log"
    cc_path_switched "$t/other.log" "http://127.0.0.1:5"
    st_case other-relay-rejected 1 $?
    # TERM을 무시하는 자식도 KILL로 끝내고 회수한다. 자식이 trap을 설치하고 준비 파일을 쓴 뒤에 정리를 시작하고,
    # KILL 분기에 들어갔는지(그 분기의 로그 줄)를 단언한다.
    bash -c 'trap "" TERM; : > "$1"; while :; do sleep 1; done' _ "$t/ready" &
    pid=$!
    CC_PEER_PIDS+=("$pid")
    for ((i = 0; i < 50; i++)); do
        [[ -e "$t/ready" ]] && break
        sleep 0.1
    done
    [[ -e "$t/ready" ]]
    st_case term-trap-installed 0 $?
    start=$SECONDS
    PEER_KILL_SECONDS=1 cc_stop_peer "$pid" 2>"$t/stop.err"
    st_case stop-term-ignoring-child 0 $?
    grep -q 'KILL한다' "$t/stop.err"
    st_case kill-branch-taken 0 $?
    cc_alive "$pid"
    st_case child-gone 1 $?
    (( SECONDS - start <= 3 ))
    st_case stop-within-limit 0 $?
    rm -rf "${t:?}"
    return "$fail"
}

cc_main() {
    local configs=() peer="" relay_only=0 src name failed=0 prep
    while [[ $# -gt 0 ]]; do
        case "$1" in
            --peer-bin)
                [[ $# -ge 2 ]] || { rc_log "--peer-bin에 경로가 없다."; return 2; }
                peer="$2"
                shift
                ;;
            --relay-only) relay_only=1 ;;
            --self-test)
                cc_self_test
                return $?
                ;;
            --keep-image) rc_keep_image ;;
            -h | --help)
                rc_help "${BASH_SOURCE[0]}"
                return 0
                ;;
            -*) rc_log "모르는 옵션: $1"; return 2 ;;
            *) configs+=("$1") ;;
        esac
        shift
    done
    if [[ $relay_only == 1 && -n "$peer" ]]; then
        rc_log "--peer-bin과 --relay-only는 함께 쓰지 않는다."
        return 2
    fi
    if [[ $relay_only == 0 ]]; then
        if [[ -z "$peer" ]]; then
            rc_log "판정 불가: --peer-bin <p2p-peer 경로>가 없다. 기동 판정만 하려면 --relay-only."
            return 2
        fi
        if [[ ! -x "$peer" ]]; then
            rc_log "판정 불가: 실행할 수 없는 p2p-peer 경로: $peer"
            return 2
        fi
    fi
    trap cc_cleanup EXIT
    rc_prepare
    prep=$?
    [[ $prep == 0 ]] || return "$prep"
    rc_log "image=$RELAY_IMAGE peer=${peer:-none}"
    while IFS= read -r src; do
        name=$(basename "$src" .toml)
        if ! rc_start_relay "$name" "$src"; then
            echo "relay-check config=$name result=fail $RC_LAST_FAIL"
            failed=1
        elif [[ $relay_only == 1 ]]; then
            echo "relay-check config=$name result=relay-only http=$RC_LAST_HTTP metrics=$RC_LAST_METRICS limit=$RC_LAST_LIMIT peers=not-run"
        elif cc_run_peers "$name" "http://$RC_LAST_HTTP" "$peer" "$RC_LAST_METRICS"; then
            echo "relay-check config=$name result=ok http=$RC_LAST_HTTP metrics=$RC_LAST_METRICS limit=$RC_LAST_LIMIT"
        else
            echo "relay-check config=$name result=fail $RC_LAST_FAIL"
            failed=1
        fi
        rc_stop_relay "$RC_LAST_CONTAINER" || failed=1
    done < <(rc_configs "${configs[@]}")
    return "$failed"
}

cc_main "$@"
exit $?
