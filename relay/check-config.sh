#!/usr/bin/env bash
# relay 설정 예시 기동 검사. 동작 수준 기준 D5 (a)(.github/workflows/relay.yml의 relay-config-check 잡)가 부른다.
#
#   relay/check-config.sh [--keep-image] [설정.toml ...]
#
# 설정을 주지 않으면 relay/punch-only.toml과 relay/relay-fallback.toml을 본다. 설정마다:
#   1. 운영 예시에서 [tls] 절과 enable_quic_addr_discovery를 뺀 CI용 설정을 임시 폴더에 만든다.
#   2. 고정 이미지(RELAY_IMAGE)를 --dev와 그 설정으로 띄운다. 호스트 쪽은 127.0.0.1의 빈 포트에 게시한다.
#   3. HTTP(/healthz, /, /generate_204)와 metrics(/metrics) 응답, 시작 로그의 평문 리스너 줄
#      ([HTTP/WS] relay: serving on), 시작 로그에 찍힌 수신 상한이 설정 파일과 같은지
#      (설정 파일을 실제로 읽었는지) 본다.
#   4. 결과를 한 줄로 쓴다: relay-check config=<이름> result=ok|fail ...
# 띄운 컨테이너와 임시 폴더는 끝날 때 지운다. 이 스크립트가 받은 이미지도 지운다(--keep-image면 남긴다).
# TLS와 QUIC 주소 확인은 판정하지 않는다(docs/design/relay.md "배포 형태").
#
# 종료 코드: 0 모든 설정 통과, 1 하나라도 실패, 2 판정 불가(docker 없음, 인자 오류).
# relay/container-check.sh가 이 파일을 source해 같은 함수를 쓴다.
#
# 환경 변수:
#   RELAY_IMAGE          기본 n0computer/iroh-relay:v1.5.0@sha256:...(아래). 바꾸면 RELAY_VERSION도 맞춘다.
#   RELAY_VERSION        /healthz가 돌려줘야 하는 판. 기본 1.5.0
#   RELAY_READY_SECONDS  기동을 기다리는 시간. 기본 30
set -uo pipefail

RELAY_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# 태그는 옮겨질 수 있어 digest(multi-arch index)를 함께 고정한다. 판을 올릴 때 relay/compose.yaml과 같이 바꾼다.
RELAY_IMAGE="${RELAY_IMAGE:-n0computer/iroh-relay:v1.5.0@sha256:50a2f5697d8f69537037503c81b989be17eeb694dbb2d3fdfd5418c9a71c8ec6}"
RELAY_VERSION="${RELAY_VERSION:-1.5.0}"
RELAY_READY_SECONDS="${RELAY_READY_SECONDS:-30}"

rc_log() { echo "[relay-check] $*" >&2; }

# 정리 대상. rc_cleanup이 EXIT에서 지운다.
RC_CONTAINERS=()
RC_TMP=""
RC_PULLED=0
RC_KEEP_IMAGE=0

# --keep-image. relay/container-check.sh도 이 함수로 정한다.
rc_keep_image() { RC_KEEP_IMAGE=1; }

rc_cleanup() {
    local c
    for c in "${RC_CONTAINERS[@]}"; do
        rc_remove_container "$c" || rc_log "컨테이너를 지우지 못했다: $c"
    done
    if [[ -n "$RC_TMP" ]]; then
        rm -rf "${RC_TMP:?}"
    fi
    if [[ "$RC_PULLED" == 1 && "$RC_KEEP_IMAGE" == 0 ]]; then
        docker image rm "$RELAY_IMAGE" >/dev/null 2>&1 || rc_log "이미지를 지우지 못했다: $RELAY_IMAGE"
    fi
    return 0
}

# 컨테이너 $1(이름 또는 ID)을 지우고, 없어졌는지 확인한다. 남았으면 1.
rc_remove_container() {
    docker rm -f "$1" >/dev/null 2>&1 || true
    ! docker container inspect "$1" >/dev/null 2>&1
}

# docker와 이미지를 준비한다. 이미지가 없을 때만 받고, 받았으면 끝에 지운다.
rc_prepare() {
    if ! command -v docker >/dev/null 2>&1; then
        rc_log "판정 불가: docker가 PATH에 없다."
        return 2
    fi
    if ! docker info >/dev/null 2>&1; then
        rc_log "판정 불가: docker 데몬에 닿지 않는다."
        return 2
    fi
    if ! command -v curl >/dev/null 2>&1; then
        rc_log "판정 불가: curl이 PATH에 없다."
        return 2
    fi
    RC_TMP="$(mktemp -d)" || { rc_log "판정 불가: 임시 폴더를 만들지 못했다."; return 2; }
    if ! docker image inspect "$RELAY_IMAGE" >/dev/null 2>&1; then
        rc_log "이미지 받기: $RELAY_IMAGE"
        docker pull -q "$RELAY_IMAGE" >/dev/null || { rc_log "판정 불가: 이미지를 받지 못했다."; return 2; }
        RC_PULLED=1
    fi
    return 0
}

# 운영 예시 $1에서 [tls]·[tls.*] 표와 enable_quic_addr_discovery 키를 뺀 설정을 $2에 쓴다.
# 표 머리 밖의 tls 키(tls.x = ..., tls = {...})는 이 변환이 다루지 않으므로 실패로 본다.
rc_make_ci_config() {
    local src="$1" out="$2"
    awk '
        /^[[:space:]]*\[/ {
            h = $0
            sub(/^[[:space:]]*\[+[[:space:]]*/, "", h)
            skip = (h ~ /^tls[[:space:]]*(\]|\.)/)
        }
        skip { next }
        /^[[:space:]]*enable_quic_addr_discovery[[:space:]]*=/ { next }
        { print }
    ' "$src" > "$out" || return 1
    if grep -Eq '^[[:space:]]*(tls[[:space:]]*[.=]|"tls"|enable_quic_addr_discovery)' "$out"; then
        rc_log "$src: 표 머리 밖의 tls 키나 enable_quic_addr_discovery가 남았다. [tls] 표로 적는다."
        return 1
    fi
    return 0
}

# TOML 파일 $1에서 [limits.client.rx] 표의 키 $2 값(정수)을 출력한다. 없으면 빈 줄.
rc_toml_rx_value() {
    awk -v key="$2" '
        /^[[:space:]]*\[/ {
            h = $0
            gsub(/[[:space:]]/, "", h)
            inrx = (h == "[limits.client.rx]")
            next
        }
        inrx {
            line = $0
            sub(/#.*/, "", line)
            if (line ~ "^[[:space:]]*" key "[[:space:]]*=") {
                sub(/^[^=]*=[[:space:]]*/, "", line)
                gsub(/[[:space:]_]/, "", line)
                print line
                exit
            }
        }
    ' "$1"
}

# 설정 값 $2(키 이름)의 포트. 없으면 기본 $3.
rc_toml_port() {
    local v
    v=$(sed -n "s/^[[:space:]]*$2[[:space:]]*=[[:space:]]*\"\\(.*\\)\".*/\\1/p" "$1" | head -1)
    if [[ -n "$v" ]]; then
        echo "${v##*:}"
    else
        echo "$3"
    fi
}

# 컨테이너 $1의 내부 TCP 포트 $2가 게시된 호스트 주소(127.0.0.1:N).
rc_host_addr() {
    docker port "$1" "$2/tcp" 2>/dev/null | grep -m1 '^127\.0\.0\.1:' || true
}

# HTTP 상태 코드를 출력한다. curl 전송이 실패하면(닿지 않음, 본문이 중간에 끊김 등) 상태 대신 curl<종료 코드>.
rc_http_code() {
    local code rc
    code=$(curl -s -o /dev/null -m 5 -w '%{http_code}' "$1" 2>/dev/null)
    rc=$?
    if [[ $rc != 0 ]]; then
        echo "curl$rc"
    else
        echo "$code"
    fi
}

# 설정 하나를 띄우고 판정한다. 인자: 이름, 운영 예시 경로.
# 성공하면 RC_LAST_CONTAINER·RC_LAST_HTTP·RC_LAST_METRICS를 채우고 컨테이너를 띄운 채 0을 돌려준다.
# 실패하면 stage와 로그를 남기고 1을 돌려준다. 컨테이너는 rc_stop_relay나 EXIT 정리가 지운다.
RC_LAST_CONTAINER=""
RC_LAST_HTTP=""
RC_LAST_METRICS=""
RC_LAST_FAIL=""
RC_LAST_LIMIT=""
rc_start_relay() {
    local name="$1" src="$2" dir cfg cid http_port metrics_port http metrics waited body code_root code_204 mbody
    local bps burst logs flat want
    RC_LAST_CONTAINER="" RC_LAST_HTTP="" RC_LAST_METRICS="" RC_LAST_FAIL="" RC_LAST_LIMIT=""
    dir="$RC_TMP/$name"
    mkdir -p "$dir" || { RC_LAST_FAIL="stage=prepare"; return 1; }
    cfg="$dir/relay.toml"
    if [[ ! -f "$src" ]]; then
        # iroh-relay는 없는 설정 경로를 오류 없이 기본값으로 대신하므로 여기서 막는다.
        RC_LAST_FAIL="stage=config reason=missing-file"
        return 1
    fi
    rc_make_ci_config "$src" "$cfg" || { RC_LAST_FAIL="stage=config reason=ci-config"; return 1; }
    chmod 0644 "$cfg"
    http_port=$(rc_toml_port "$cfg" http_bind_addr 3340)
    metrics_port=$(rc_toml_port "$cfg" metrics_bind_addr 9090)

    # 이름을 먼저 정리 목록에 넣는다. create가 컨테이너를 만든 뒤 실패해도, start가 실패해도 EXIT 정리가 지운다.
    cid="relay-check-$name-$$"
    RC_CONTAINERS+=("$cid")
    RC_LAST_CONTAINER="$cid"
    if ! docker create \
        --name "$cid" \
        -e RUST_LOG=iroh_relay=debug -e NO_COLOR=1 \
        -v "$cfg:/config/relay.toml:ro" \
        -p "127.0.0.1::$http_port" -p "127.0.0.1::$metrics_port" \
        "$RELAY_IMAGE" --dev --config-path /config/relay.toml >/dev/null; then
        RC_LAST_FAIL="stage=start reason=docker-create"
        return 1
    fi
    if ! docker start "$cid" >/dev/null; then
        RC_LAST_FAIL="stage=start reason=docker-start"
        rc_dump_logs "$cid"
        return 1
    fi

    http=$(rc_host_addr "$cid" "$http_port")
    metrics=$(rc_host_addr "$cid" "$metrics_port")
    if [[ -z "$http" || -z "$metrics" ]]; then
        RC_LAST_FAIL="stage=start reason=no-published-port"
        rc_dump_logs "$cid"
        return 1
    fi

    body=""
    for ((waited = 0; waited < RELAY_READY_SECONDS; waited++)); do
        if [[ "$(docker inspect -f '{{.State.Running}}' "$cid" 2>/dev/null)" != true ]]; then
            RC_LAST_FAIL="stage=start reason=exited"
            rc_dump_logs "$cid"
            return 1
        fi
        body=$(curl -fsS -m 2 "http://$http/healthz" 2>/dev/null) && break
        body=""
        sleep 1
    done
    if [[ -z "$body" ]]; then
        RC_LAST_FAIL="stage=http reason=not-ready-${RELAY_READY_SECONDS}s"
        rc_dump_logs "$cid"
        return 1
    fi
    if ! grep -q '"status":"ok"' <<<"$body" || ! grep -q "\"version\":\"$RELAY_VERSION\"" <<<"$body"; then
        RC_LAST_FAIL="stage=http reason=healthz body=$body"
        rc_dump_logs "$cid"
        return 1
    fi
    code_root=$(rc_http_code "http://$http/")
    code_204=$(rc_http_code "http://$http/generate_204")
    if [[ "$code_root" != 200 || "$code_204" != 204 ]]; then
        RC_LAST_FAIL="stage=http root=$code_root generate_204=$code_204"
        rc_dump_logs "$cid"
        return 1
    fi

    # 본문을 변수로 다 받은 뒤 본다(curl 종료 코드를 잃지 않게).
    if ! mbody=$(curl -fsS -m 5 "http://$metrics/metrics" 2>/dev/null); then
        RC_LAST_FAIL="stage=metrics code=$(rc_http_code "http://$metrics/metrics")"
        rc_dump_logs "$cid"
        return 1
    fi
    if ! grep -q '^relayserver_accepts_total ' <<<"$mbody"; then
        RC_LAST_FAIL="stage=metrics reason=no-accepts-counter"
        rc_dump_logs "$cid"
        return 1
    fi

    # 시작 로그(debug의 ServerConfig {:#?} 출력)에 설정 파일의 수신 상한이 그대로 있어야 한다.
    # 여러 줄로 찍히고 닫는 괄호 앞에 쉼표가 붙으므로, 공백·줄바꿈과 그 쉼표를 지운 뒤 비교한다.
    bps=$(rc_toml_rx_value "$cfg" bytes_per_second)
    burst=$(rc_toml_rx_value "$cfg" max_burst_bytes)
    logs=$(docker logs "$cid" 2>&1)
    flat=$(tr -d ' \t\n' <<<"$logs" | sed 's/,)/)/g; s/,}/}/g')
    if [[ -n "$bps" ]]; then
        if [[ -n "$burst" ]]; then
            want="client_rx:Some(ClientRateLimit{bytes_per_second:$bps,max_burst_bytes:Some($burst)}"
        else
            want="client_rx:Some(ClientRateLimit{bytes_per_second:$bps,max_burst_bytes:None}"
        fi
    else
        want="client_rx:None,"
    fi
    if ! grep -Fq "$want" <<<"$flat"; then
        RC_LAST_FAIL="stage=limits reason=log-without-$want"
        rc_dump_logs "$cid"
        return 1
    fi
    # TLS 없는 --dev 기동은 relay 리스너를 평문으로 연다(iroh-relay v1.5.0 server/http_server.rs:467-468).
    if ! grep -Fq "[HTTP/WS] relay: serving on [::]:$http_port" <<<"$logs"; then
        RC_LAST_FAIL="stage=log reason=no-http-listener-line"
        rc_dump_logs "$cid"
        return 1
    fi
    if grep -Eq '(^|[^A-Za-z_])ERROR([^A-Za-z_]|$)' <<<"$logs"; then
        RC_LAST_FAIL="stage=log reason=error-line"
        rc_dump_logs "$cid"
        return 1
    fi

    RC_LAST_HTTP="$http"
    RC_LAST_METRICS="$metrics"
    RC_LAST_LIMIT="${bps:-none}"
    return 0
}

rc_dump_logs() {
    rc_log "컨테이너 로그(끝 40줄):"
    docker logs "$1" 2>&1 | tail -40 | sed 's/^/    /' >&2
}

# rc_start_relay가 띄운 컨테이너를 끝내고 지운다. 없어진 것이 확인될 때만 정리 목록에서 뺀다.
# 남으면 목록에 둬서 EXIT 정리가 다시 지운다.
rc_stop_relay() {
    local cid="$1" i
    [[ -n "$cid" ]] || return 0
    if ! rc_remove_container "$cid"; then
        rc_log "컨테이너가 남았다: $cid (끝날 때 다시 지운다)"
        return 1
    fi
    for i in "${!RC_CONTAINERS[@]}"; do
        [[ "${RC_CONTAINERS[$i]}" == "$cid" ]] && unset 'RC_CONTAINERS[i]'
    done
    return 0
}

# 인자로 받은 설정 목록. 없으면 두 운영 예시.
rc_configs() {
    if [[ $# -gt 0 ]]; then
        printf '%s\n' "$@"
    else
        printf '%s\n' "$RELAY_DIR/punch-only.toml" "$RELAY_DIR/relay-fallback.toml"
    fi
}

rc_main() {
    local configs=() src name failed=0 prep
    while [[ $# -gt 0 ]]; do
        case "$1" in
            --keep-image) rc_keep_image ;;
            -h | --help)
                sed -n '2,21p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
                return 0
                ;;
            -*) rc_log "모르는 옵션: $1"; return 2 ;;
            *) configs+=("$1") ;;
        esac
        shift
    done
    trap rc_cleanup EXIT
    rc_prepare
    prep=$?
    [[ $prep == 0 ]] || return "$prep"
    rc_log "image=$RELAY_IMAGE"
    while IFS= read -r src; do
        name=$(basename "$src" .toml)
        if rc_start_relay "$name" "$src"; then
            echo "relay-check config=$name result=ok http=$RC_LAST_HTTP metrics=$RC_LAST_METRICS limit=$RC_LAST_LIMIT"
        else
            echo "relay-check config=$name result=fail $RC_LAST_FAIL"
            failed=1
        fi
        rc_stop_relay "$RC_LAST_CONTAINER" || failed=1
    done < <(rc_configs "${configs[@]}")
    return "$failed"
}

if [[ "${BASH_SOURCE[0]}" == "$0" ]]; then
    rc_main "$@"
    exit $?
fi
