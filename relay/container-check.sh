#!/usr/bin/env bash
# 실제 relay 프로세스(컨테이너)를 거친 P2P 연결 확인. 동작 수준 기준 D5 (b)다. CI가 아니라 원격 빌드 환경에서 돌린다.
#
#   relay/container-check.sh --peer-bin <p2p-peer 경로> [--keep-image] [설정.toml ...]
#   relay/container-check.sh --relay-only [--keep-image] [설정.toml ...]
#
# 설정마다 relay/check-config.sh와 같은 CI용 설정(TLS·QUIC 주소 확인 제외)으로 고정 이미지를 띄우고
# 같은 기동 판정(HTTP·metrics·수신 상한)을 한 뒤, p2p-peer 두 개를 그 relay에 붙인다.
# 결과 줄:
#   relay-check config=<이름> result=ok ...           기동 판정과 p2p-peer 연결이 모두 통과
#   relay-check config=<이름> result=relay-only ...   --relay-only: 기동 판정만 통과, p2p-peer는 돌리지 않음(D5 (b) 미충족)
#   relay-check config=<이름> result=fail ...         어느 단계든 실패
# p2p-peer 로그의 경로 바뀜 줄은 "relay-check config=<이름> peer-log: ..."로 옮긴다.
# 띄운 컨테이너·프로세스·임시 폴더는 끝날 때 지운다. 이 스크립트가 받은 이미지도 지운다(--keep-image면 남긴다).
#
# 종료 코드: 0 모든 설정 통과(--relay-only면 기동 판정만), 1 하나라도 실패, 2 판정 불가(docker·p2p-peer 없음, 인자 오류).
#
# 환경 변수: relay/check-config.sh의 것과 같고, 더해서
#   PEER_TIMEOUT_SECONDS  dialer가 끝나기를 기다리는 시간. 기본 60
set -uo pipefail

# shellcheck source=relay/check-config.sh
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/check-config.sh"

PEER_TIMEOUT_SECONDS="${PEER_TIMEOUT_SECONDS:-60}"
CC_PEER_PIDS=()

cc_cleanup() {
    local p
    for p in "${CC_PEER_PIDS[@]}"; do
        kill "$p" 2>/dev/null || true
    done
    rc_cleanup
}

# p2p-peer 두 개를 relay URL $2에 붙인다. 인자: 이름, relay URL, p2p-peer 경로.
#
# p2p-peer는 아직 없다(crates/tasty-net-demo, P2P 작업이 만든다). 아래는 이 스크립트가 가정한 인터페이스다.
# p2p-peer가 생기면 그 실제 옵션·출력에 맞춰 이 함수만 고친다.
#   listener: p2p-peer --listen --relay-url <URL> --key-dir <폴더> --addr-file <파일>
#             자기 주소(dialer가 --dial에 그대로 넘길 문자열)를 <파일>에 쓰고 연결 하나를 받는다.
#   dialer:   p2p-peer --dial <주소> --relay-url <URL> --key-dir <폴더>
#             relay를 거쳐 연결하고, 선택 경로가 직접으로 바뀌면 "path=direct"가 든 줄을 쓰고 0으로 끝난다.
cc_run_peers() {
    local name="$1" url="$2" peer="$3" dir pid waited rc lines
    dir="$RC_TMP/$name/peers"
    mkdir -p "$dir/a" "$dir/b" || return 1
    "$peer" --listen --relay-url "$url" --key-dir "$dir/a" --addr-file "$dir/addr" > "$dir/listen.log" 2>&1 &
    pid=$!
    CC_PEER_PIDS+=("$pid")
    for ((waited = 0; waited < 30; waited++)); do
        [[ -s "$dir/addr" ]] && break
        kill -0 "$pid" 2>/dev/null || break
        sleep 1
    done
    if [[ ! -s "$dir/addr" ]]; then
        RC_LAST_FAIL="stage=peers reason=no-listener-addr"
        sed 's/^/    /' "$dir/listen.log" >&2
        cc_stop_peer "$pid"
        return 1
    fi
    timeout "$PEER_TIMEOUT_SECONDS" "$peer" --dial "$(cat "$dir/addr")" --relay-url "$url" --key-dir "$dir/b" > "$dir/dial.log" 2>&1
    rc=$?
    cc_stop_peer "$pid" || rc=1
    lines=$(grep -h 'path=' "$dir/dial.log" "$dir/listen.log" || true)
    if [[ -n "$lines" ]]; then
        while IFS= read -r l; do
            echo "relay-check config=$name peer-log: $l"
        done <<<"$lines"
    fi
    if [[ $rc != 0 ]] || ! grep -q 'path=direct' "$dir/dial.log"; then
        RC_LAST_FAIL="stage=peers dial_rc=$rc"
        rc_log "dialer 로그(끝 40줄):"
        tail -40 "$dir/dial.log" | sed 's/^/    /' >&2
        return 1
    fi
    return 0
}

# PID로 끝내고 남지 않았는지 본다.
cc_stop_peer() {
    local pid="$1" i
    kill "$pid" 2>/dev/null || true
    wait "$pid" 2>/dev/null || true
    for i in "${!CC_PEER_PIDS[@]}"; do
        [[ "${CC_PEER_PIDS[$i]}" == "$pid" ]] && unset 'CC_PEER_PIDS[i]'
    done
    if kill -0 "$pid" 2>/dev/null; then
        rc_log "p2p-peer가 남았다: pid $pid"
        return 1
    fi
    return 0
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
            --keep-image) rc_keep_image ;;
            -h | --help)
                sed -n '2,20p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
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
        elif cc_run_peers "$name" "http://$RC_LAST_HTTP" "$peer"; then
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
