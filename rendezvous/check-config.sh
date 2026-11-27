#!/usr/bin/env bash
# rendezvous 설정 예시 검사. 동작 수준 기준 D5 (a)(.github/workflows/rendezvous.yml의 rendezvous-check 잡)가 부른다.
#
#   rendezvous/check-config.sh                예시 설정(rendezvous/rendezvous.toml)을 검사한다
#   rendezvous/check-config.sh --self-test    서버 없이 이 스크립트의 판정 함수만 시험한다
#
# 검사 순서:
#   1. tasty-rendezvous와 stun_probe 예제를 빌드한다(RENDEZVOUS_BIN·STUN_PROBE를 주면 그 파일을 쓴다).
#   2. 예시를 임시 폴더에 복사하고 key_dir 한 줄만 임시 키 폴더로 바꾼다. keygen으로 서버 키를 만들고,
#      다시 부르면 같은 키를 두는지 본다.
#   3. settings --config: 줄마다 "키 = 값 (출처)" 형식인지, 예시에 적은 키만 출처가 configured이고 값이 같은지,
#      key_dir·access 줄이 맞는지 본다. 주석으로 적은 기본값을 풀어 쓴 사본은 그 키가 configured가 되고
#      값이 기본과 같아야 한다. 접근 정책 주석 두 가지(everyone, allowlist)도 풀어서 읽힌다.
#   4. 서버를 띄워 시작 로그(공개키, 설정 줄, wildcard 경고)를 보고, 주 포트와 보조 포트에 STUN Binding을
#      보내 반사 주소가 보낸 소켓 주소와 같은지 본다.
#   5. SIGINT로 멈추고 종료 코드 0과 종료 로그를 본다.
#   6. 결과를 rendezvous-check step=<단계> ok 줄들과 마지막 줄 rendezvous-check result=ok|fail로 쓴다.
# 예시의 포트(기본 UDP 3478·3479)를 그대로 쓰므로 그 포트가 비어 있어야 한다.
# 임시 폴더와 띄운 서버는 끝날 때 지운다.
#
# 종료 코드: 0 통과, 1 실패, 2 판정 불가(빌드 실패, 인자 오류).
#
# 환경 변수:
#   RENDEZVOUS_BIN                 tasty-rendezvous 실행 파일. 주면 빌드하지 않는다(STUN_PROBE와 함께 준다)
#   STUN_PROBE                     stun_probe 예제 실행 파일
#   RENDEZVOUS_START_SECONDS       서버 시작 로그(시작 줄, 설정 줄, wildcard 경고)를 다 기다리는 시간. 기본 30
#   RENDEZVOUS_STUN_TIMEOUT_MS     STUN 응답 하나를 기다리는 시간. 기본 2000
#   RENDEZVOUS_STOP_SECONDS        SIGINT 뒤 서버가 끝나기를 기다리는 시간. 넘으면 SIGKILL하고 실패. 기본 10
#   RENDEZVOUS_CLEANUP_SECONDS     실패 경로 정리에서 SIGTERM 뒤 기다리는 시간. 넘으면 SIGKILL. 기본 5
set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"
EXAMPLE="$HERE/rendezvous.toml"
START_SECONDS="${RENDEZVOUS_START_SECONDS:-30}"
STUN_TIMEOUT_MS="${RENDEZVOUS_STUN_TIMEOUT_MS:-2000}"
STOP_SECONDS="${RENDEZVOUS_STOP_SECONDS:-10}"
CLEANUP_SECONDS="${RENDEZVOUS_CLEANUP_SECONDS:-5}"
# 기다리는 동안 다시 보는 간격(초).
POLL_SECONDS=0.1

# 빌드 명령. json-render-diagnostics는 산출물 JSON을 stdout에 내고 컴파일 오류 본문은 사람이 읽는
# 형식으로 stderr에 남긴다(json만 쓰면 오류 본문이 stdout JSON에 묻혀 로그에서 사라진다).
BUILD_CMD=(cargo build --locked --message-format=json-render-diagnostics -p tasty-net-rendezvous
    --bin tasty-rendezvous --example stun_probe)

log() { echo "[rendezvous-check] $*" >&2; }

# 파일 머리의 주석 블록(둘째 줄부터 첫 비주석 줄 전까지)을 도움말로 출력한다.
show_help() { awk 'NR > 1 && !/^#/ { exit } NR > 1 { sub(/^# ?/, ""); print }' "$0"; }

# 지금 시각(마이크로초). bash 5의 EPOCHREALTIME을 쓰고, 없으면 date의 나노초에서 얻는다.
# date가 %N을 모르면(BSD date는 1764230400N처럼 글자를 그대로 낸다) 초를 마이크로초로 바꿔 쓴다.
# 그때 해상도는 1초다.
now_us() {
    local t=${EPOCHREALTIME:-}
    if [[ -n "$t" ]]; then
        t=${t/[.,]/}
    else
        t=$(date +%s%N)
        if [[ "$t" =~ ^[0-9]+$ ]]; then
            t=${t%???}
        else
            t=$(($(date +%s) * 1000000))
        fi
    fi
    echo "$((10#$t))"
}

# wait_until <초> <명령...>: 명령이 성공할 때까지 POLL_SECONDS마다 다시 부른다. 기한은 부른 횟수가
# 아니라 경과 시각으로 잰다(명령과 sleep에 걸린 시간도 기한에 든다).
# 기한 안에 성공하면 0, 기한을 넘기면 1. 성공도 기한 안에서만 성공이라, 명령이 성공한 뒤 시각을
# 다시 재서 기한을 넘었으면 1이다.
wait_until() {
    local limit=$1 end
    shift
    end=$(($(now_us) + $(awk -v s="$limit" 'BEGIN { printf "%d", s * 1000000 }')))
    until "$@"; do
        [[ "$(now_us)" -lt "$end" ]] || return 1
        sleep "$POLL_SECONDS"
    done
    [[ "$(now_us)" -le "$end" ]]
}

# 프로세스 $1이 없다(bash는 끝난 자식을 바로 거두므로 끝난 자식도 없는 것으로 보인다).
gone() { ! kill -0 "$1" 2>/dev/null; }

# stop_process <pid> <신호> <초>: 신호를 보내고 기한 안에 끝나기를 기다린다. 끝나면 0이고 종료 코드는
# STOP_RC다. 기한을 넘기면 SIGKILL로 끝내고 거둔 뒤 1이다(STOP_RC는 강제 종료의 코드).
STOP_RC=""
stop_process() {
    local pid=$1 sig=$2 limit=$3
    kill -s "$sig" "$pid" 2>/dev/null
    if wait_until "$limit" gone "$pid"; then
        STOP_RC=0
        wait "$pid" 2>/dev/null || STOP_RC=$?
        return 0
    fi
    kill -s KILL "$pid" 2>/dev/null
    STOP_RC=0
    wait "$pid" 2>/dev/null || STOP_RC=$?
    return 1
}

TMP=""
SERVER_PID=""
cleanup() {
    if [[ -n "$SERVER_PID" ]] && ! gone "$SERVER_PID"; then
        stop_process "$SERVER_PID" TERM "$CLEANUP_SECONDS" \
            || log "정리: 서버(pid $SERVER_PID)가 SIGTERM 뒤 ${CLEANUP_SECONDS}s 안에 끝나지 않아 SIGKILL로 끝냈다"
        SERVER_PID=""
    fi
    if [[ -n "$TMP" ]]; then
        rm -rf "${TMP:?}"
    fi
}
trap cleanup EXIT

# ── 판정 함수(자체 시험 대상) ────────────────────────────────────────────

# 설정 파일 $1에서 표 안에 주석 없이 적은 키를 "표.키 값" 줄로 낸다. 값의 따옴표는 뗀다.
# 맨 위 key_dir와 [access] 표는 settings 출력에서 따로 보이므로 빼고 낸다.
configured_keys() {
    awk '
        /^\[[a-z_]+\][ \t]*$/ { t = $0; gsub(/[][ \t]/, "", t); next }
        /^[a-z_]+[ \t]*=/ {
            if (t == "" || t == "access") next
            k = $0; sub(/[ \t]*=.*/, "", k)
            v = $0; sub(/^[^=]*=[ \t]*/, "", v); sub(/[ \t]+#.*$/, "", v); gsub(/"/, "", v)
            print t "." k " " v
        }
    ' "$1"
}

# 설정 파일 $1에서 표 [listen]·[advertise] 안의 주석 처리한 "# 키 = 값" 줄의 주석만 풀어 낸다.
# 주석으로 시작하는 표("# [access]") 아래 줄은 풀지 않는다.
uncomment_defaults() {
    awk '
        /^\[[a-z_]+\][ \t]*$/ { t = $0; gsub(/[][ \t]/, "", t); print; next }
        /^# \[/ { t = ""; print; next }
        /^# [a-z_]+ = / { if (t == "listen" || t == "advertise") { sub(/^# /, "") } print; next }
        { print }
    ' "$1"
}

# 설정 파일 $1의 접근 정책 주석 중 policy = "$2"인 묶음("# [access]"와 그 아래 "# 키 = 값" 줄들)만 푼다.
uncomment_access() {
    awk -v want="$2" '
        { line[NR] = $0 }
        END {
            for (i = 1; i <= NR; i++) {
                if (line[i] != "# [access]") continue
                j = i + 1
                if (line[j] != "# policy = \"" want "\"") continue
                line[i] = "[access]"
                for (; j <= NR && line[j] ~ /^# [a-z_]+ = /; j++) sub(/^# /, "", line[j])
            }
            for (i = 1; i <= NR; i++) print line[i]
        }
    ' "$1"
}

# 설정 파일 $1의 key_dir 한 줄을 $2로 바꿔 낸다. key_dir 줄이 정확히 하나가 아니면 실패한다.
set_key_dir() {
    local n
    n=$(grep -c '^key_dir[ \t]*=' "$1")
    [[ "$n" == 1 ]] || { log "key_dir 줄이 ${n}개다(하나여야 한다): $1"; return 1; }
    awk -v d="$2" '/^key_dir[ \t]*=/ { print "key_dir = \"" d "\""; next } { print }' "$1"
}

# settings 출력 $1에서 키 $2의 "값 (출처)"를 낸다.
setting_of() {
    awk -v k="$2" 'index($0, k " = ") == 1 { print substr($0, length(k) + 4); exit }' <<<"$1"
}

# settings 출력 $1의 설정 줄 형식을 본다. key_dir·access 줄을 뺀 모든 줄이 "키 = 값 (출처)"여야 하고,
# 출처는 default·configured·fixed·computed 중 하나다. 줄이 없어도 실패다.
check_settings_format() {
    local bad n
    n=$(grep -vcE '^(key_dir|access) = ' <<<"$1")
    [[ "$n" -gt 0 ]] || { log "설정 줄이 없다"; return 1; }
    bad=$(grep -vE '^(key_dir|access) = ' <<<"$1" \
        | grep -vE '^[a-z0-9_.]+ = .+ \((default|configured|fixed|computed)\)$' || true)
    [[ -z "$bad" ]] || { log "형식이 다른 설정 줄: $bad"; return 1; }
}

# 값이 주소·포트·숫자 모양일 때만 글자를 비교한다. 시간·크기는 표기가 달라도 같은 값일 수 있다.
COMPARABLE_RE='^[][0-9A-Fa-f.:]+$'
comparable_value() { [[ "$1" =~ $COMPARABLE_RE ]]; }

# settings 출력 $1이 설정 파일 $2와 맞는지 본다: 파일에 적은 키만 출처가 configured이고 값이 같다.
check_configured() {
    local out=$1 file=$2 want got key value entry fail=0
    want=$(configured_keys "$file" | awk '{ print $1 }' | sort)
    got=$(grep -E ' \(configured\)$' <<<"$out" | awk '{ print $1 }' | sort)
    if [[ "$want" != "$got" ]]; then
        log "출처 configured인 키가 파일과 다르다. 파일: [$(tr '\n' ' ' <<<"$want")] 출력: [$(tr '\n' ' ' <<<"$got")]"
        fail=1
    fi
    while read -r key value; do
        [[ -n "$key" ]] || continue
        entry=$(setting_of "$out" "$key")
        if comparable_value "$value" && [[ "$entry" != "$value (configured)" ]]; then
            log "$key: 파일 값 $value, 출력 $entry"
            fail=1
        fi
    done < <(configured_keys "$file")
    return "$fail"
}

# 기본값 주석을 푼 사본의 settings 출력 $2가 원본 출력 $1과 같은 값인지 본다. 키 목록은 파일 $3(사본).
# 원본의 값이 "same as <다른 키>"면 그 키의 값과 비교한다.
check_defaults_match() {
    local plain=$1 opened=$2 file=$3 key value base other fail=0
    while read -r key value; do
        [[ -n "$key" ]] || continue
        base=$(setting_of "$plain" "$key")
        base=${base% (*}
        if [[ "$base" == "same as "* ]]; then
            other=$(setting_of "$plain" "${base#same as }")
            base=${other% (*}
        fi
        if [[ "$(setting_of "$opened" "$key")" != "$base (configured)" ]]; then
            log "$key: 주석의 값을 풀면 $(setting_of "$opened" "$key"), 기본은 $base"
            fail=1
        fi
    done < <(configured_keys "$file")
    return "$fail"
}

# cargo build --message-format=json 출력 $1에서 target 이름 $2의 실행 파일 경로를 낸다.
executable_of() {
    grep -F '"reason":"compiler-artifact"' <<<"$1" \
        | grep -F "\"name\":\"$2\"" \
        | grep -o '"executable":"[^"]*"' \
        | sed 's/^"executable":"//; s/"$//' \
        | tail -n 1
}

# missing_lines <로그> <글자...>: 로그에 없는 글자를 ; 로 이어 낸다(고정 글자 비교).
missing_lines() {
    local file=$1 p out=""
    shift
    for p in "$@"; do
        grep -qF -- "$p" "$file" 2>/dev/null || out+="${out:+; }$p"
    done
    printf '%s' "$out"
}

# wait_start_log <로그> <pid> <초> <글자...>: 로그에 모든 글자가 나올 때까지 기다린다.
# 다 나오면 0, 기한을 넘기면 1, 그 전에 프로세스가 끝나면 2(끝난 뒤의 로그를 한 번 더 본다).
wait_start_log() {
    local file=$1 pid=$2 limit=$3
    shift 3
    all_logged() { [[ -z "$(missing_lines "$file" "$@")" ]] || gone "$pid"; }
    wait_until "$limit" all_logged "$@" || return 1
    [[ -z "$(missing_lines "$file" "$@")" ]] || return 2
}

# 주소 $1(IP:포트 또는 [IPv6]:포트)의 포트.
port_of() { echo "${1##*:}"; }

# 듣는 주소 $1의 IP로 같은 머신에서 보낼 대상 IP. wildcard면 루프백이다.
probe_ip() {
    local ip=${1%:*}
    case "$ip" in
        0.0.0.0) echo 127.0.0.1 ;;
        "[::]") echo "[::1]" ;;
        *) echo "$ip" ;;
    esac
}

# stun_probe 출력 줄 $1이 서버 $2의 응답이고 반사 주소가 보낸 소켓 주소와 같은지 본다.
# 같은 머신의 루프백이라 서버가 본 출발 주소가 곧 소켓 주소다.
check_probe_line() {
    local line=$1 server=$2 local_addr mapped
    if ! [[ "$line" =~ ^stun-probe\ server=([^ ]+)\ local=([^ ]+)\ mapped=([^ ]+)$ ]]; then
        log "STUN 응답 줄이 아니다: $line"
        return 1
    fi
    [[ "${BASH_REMATCH[1]}" == "$server" ]] || { log "응답 줄의 서버 ${BASH_REMATCH[1]}, 보낸 곳 $server"; return 1; }
    local_addr=${BASH_REMATCH[2]}
    mapped=${BASH_REMATCH[3]}
    # 0.0.0.0으로 묶은 소켓은 로컬 주소의 IP가 비어 보이므로 포트만 비교한다.
    [[ "$(port_of "$mapped")" == "$(port_of "$local_addr")" ]] \
        || { log "반사 주소 $mapped 의 포트가 소켓 $local_addr 와 다르다"; return 1; }
    [[ "${mapped%:*}" == "$(probe_ip "$server")" || "${mapped%:*}" == "${server%:*}" ]] \
        || { log "반사 주소 $mapped 의 IP가 보낸 곳 $server 와 맞지 않는다"; return 1; }
}

# ── 자체 시험 ────────────────────────────────────────────────────────

self_test() {
    local t fails=0 out
    t=$(mktemp -d) || return 2
    TMP=$t
    expect() { # expect <ok|fail> <설명> <명령...>
        local want=$1 what=$2
        shift 2
        if "$@" >/dev/null 2>&1; then got=ok; else got=fail; fi
        if [[ "$got" == "$want" ]]; then echo "ok   $what"; else echo "FAIL $what (기대 $want, 결과 $got)"; fails=$((fails + 1)); fi
    }
    cat >"$t/a.toml" <<'EOF'
key_dir = "/keys"

[listen]
# primary = "0.0.0.0:3478"
# alternate_port = 3479

[advertise]
primary_ip = "203.0.113.10" # 문서용 주소
# alternate_port = 3479

# [access]
# policy = "everyone"
#
# [access]
# policy = "allowlist"
# file = "allowlist.txt"
EOF
    local plain='listen.primary = 0.0.0.0:3478 (default)
listen.alternate_port = 3479 (default)
advertise.primary_ip = 203.0.113.10 (configured)
advertise.alternate_port = same as listen.alternate_port (default)
quic.max_bidi_streams = 2 (default)
punch_session_ttl = 21 s (computed)
key_dir = /keys
access = everyone'
    local opened='listen.primary = 0.0.0.0:3478 (configured)
listen.alternate_port = 3479 (configured)
advertise.primary_ip = 203.0.113.10 (configured)
advertise.alternate_port = 3479 (configured)
quic.max_bidi_streams = 2 (default)
punch_session_ttl = 21 s (computed)
key_dir = /keys
access = everyone'

    out=$(configured_keys "$t/a.toml")
    expect ok "configured_keys: 표 안의 주석 없는 키만, 따옴표와 끝 주석을 뗀다" test "$out" == "advertise.primary_ip 203.0.113.10"

    uncomment_defaults "$t/a.toml" >"$t/b.toml"
    out=$(configured_keys "$t/b.toml" | awk '{ print $1 }' | tr '\n' ' ')
    expect ok "uncomment_defaults: [listen]·[advertise]의 주석만 푼다" test "$out" == "listen.primary listen.alternate_port advertise.primary_ip advertise.alternate_port "
    expect ok "uncomment_defaults: 접근 정책 주석은 그대로" test "$(grep -c '^# policy' "$t/b.toml")" == 2

    uncomment_access "$t/a.toml" allowlist >"$t/c.toml"
    expect ok "uncomment_access allowlist: 그 묶음의 표 머리와 키를 푼다" grep -qx 'file = "allowlist.txt"' "$t/c.toml"
    expect ok "uncomment_access allowlist: [access]는 하나만" test "$(grep -cx '\[access\]' "$t/c.toml")" == 1
    expect ok "uncomment_access allowlist: everyone 묶음은 주석으로 남는다" grep -qx '# policy = "everyone"' "$t/c.toml"
    uncomment_access "$t/a.toml" everyone >"$t/d.toml"
    expect ok "uncomment_access everyone: 빈 주석 줄에서 멈춘다" grep -qx '# policy = "allowlist"' "$t/d.toml"
    expect ok "uncomment_access everyone: 그 묶음을 푼다" grep -qx 'policy = "everyone"' "$t/d.toml"

    set_key_dir "$t/a.toml" /tmp/k >"$t/e.toml"
    expect ok "set_key_dir: key_dir 줄만 바꾼다" test "$(diff "$t/a.toml" "$t/e.toml" | grep -c '^[<>]')" == 2
    printf 'key_dir = "a"\nkey_dir = "b"\n' >"$t/f.toml"
    expect fail "set_key_dir: key_dir가 둘이면 실패" set_key_dir "$t/f.toml" /tmp/k
    printf '[listen]\n' >"$t/g.toml"
    expect fail "set_key_dir: key_dir가 없으면 실패" set_key_dir "$t/g.toml" /tmp/k

    expect ok "setting_of: 값과 출처" test "$(setting_of "$plain" listen.primary)" == "0.0.0.0:3478 (default)"
    expect ok "setting_of: 앞머리가 같은 다른 키와 섞이지 않는다" test "$(setting_of "$plain" listen.alternate_port)" == "3479 (default)"

    expect ok "check_settings_format: 정상 출력" check_settings_format "$plain"
    expect fail "check_settings_format: 출처 없는 줄" check_settings_format "$plain
listen.primary = 0.0.0.0:3478"
    expect fail "check_settings_format: 모르는 출처" check_settings_format "$plain
listen.primary = 0.0.0.0:3478 (guess)"
    expect fail "check_settings_format: 설정 줄 없음" check_settings_format 'key_dir = /keys
access = everyone'

    expect ok "check_configured: 파일과 같은 출력" check_configured "$plain" "$t/a.toml"
    expect fail "check_configured: 파일에 적은 키가 default로 나옴" check_configured "${plain/203.0.113.10 (configured)/203.0.113.10 (default)}" "$t/a.toml"
    expect fail "check_configured: 파일에 없는 키가 configured로 나옴" check_configured "${plain/2 (default)/2 (configured)}" "$t/a.toml"
    expect fail "check_configured: 값이 다름" check_configured "${plain/203.0.113.10 (configured)/203.0.113.11 (configured)}" "$t/a.toml"

    expect ok "check_defaults_match: 풀어 쓴 값이 기본과 같다(same as 포함)" check_defaults_match "$plain" "$opened" "$t/b.toml"
    expect fail "check_defaults_match: 풀어 쓴 값이 기본과 다르다" check_defaults_match "$plain" "${opened/3479 (configured)/3480 (configured)}" "$t/b.toml"
    expect fail "check_defaults_match: 풀었는데 출처가 default" check_defaults_match "$plain" "${opened/0.0.0.0:3478 (configured)/0.0.0.0:3478 (default)}" "$t/b.toml"

    local cargo_json='{"reason":"compiler-artifact","target":{"kind":["lib"],"name":"tasty_net_rendezvous"},"executable":null}
{"reason":"compiler-artifact","target":{"kind":["bin"],"name":"tasty-rendezvous"},"executable":"/t/debug/tasty-rendezvous"}
{"reason":"compiler-artifact","target":{"kind":["example"],"name":"stun_probe"},"executable":"/t/debug/examples/stun_probe"}
{"reason":"build-finished","success":true}'
    expect ok "executable_of: 실행 파일" test "$(executable_of "$cargo_json" tasty-rendezvous)" == /t/debug/tasty-rendezvous
    expect ok "executable_of: 예제" test "$(executable_of "$cargo_json" stun_probe)" == /t/debug/examples/stun_probe
    expect ok "executable_of: 없는 이름은 빈 값" test -z "$(executable_of "$cargo_json" p2p_peer)"

    expect ok "BUILD_CMD: 컴파일 오류 본문을 stderr에 남기는 형식" test "${BUILD_CMD[3]}" == --message-format=json-render-diagnostics
    expect ok "BUILD_CMD: 산출물 줄을 함께 받는 json 계열 형식 하나만" test "$(printf '%s\n' "${BUILD_CMD[@]}" | grep -c '^--message-format=')" == 1

    expect ok "port_of: IPv4" test "$(port_of 0.0.0.0:3478)" == 3478
    expect ok "port_of: IPv6" test "$(port_of '[::]:3479')" == 3479
    expect ok "probe_ip: IPv4 wildcard는 루프백" test "$(probe_ip 0.0.0.0:3478)" == 127.0.0.1
    expect ok "probe_ip: IPv6 wildcard는 루프백" test "$(probe_ip '[::]:3478')" == "[::1]"
    expect ok "probe_ip: 특정 IP는 그대로" test "$(probe_ip 192.0.2.1:3478)" == 192.0.2.1

    expect ok "check_probe_line: 정상 응답" check_probe_line 'stun-probe server=127.0.0.1:3478 local=0.0.0.0:51000 mapped=127.0.0.1:51000' 127.0.0.1:3478
    expect fail "check_probe_line: 시간 초과 줄" check_probe_line 'stun-probe server=127.0.0.1:3478 result=timeout skipped=0' 127.0.0.1:3478
    expect fail "check_probe_line: 다른 서버의 줄" check_probe_line 'stun-probe server=127.0.0.1:3479 local=0.0.0.0:51000 mapped=127.0.0.1:51000' 127.0.0.1:3478
    expect fail "check_probe_line: 반사 포트가 다르다" check_probe_line 'stun-probe server=127.0.0.1:3478 local=0.0.0.0:51000 mapped=127.0.0.1:51001' 127.0.0.1:3478
    expect fail "check_probe_line: 반사 IP가 다르다" check_probe_line 'stun-probe server=127.0.0.1:3478 local=0.0.0.0:51000 mapped=198.51.100.1:51000' 127.0.0.1:3478
    expect fail "check_probe_line: 빈 줄" check_probe_line '' 127.0.0.1:3478

    # 서버 종료의 기한. 가짜 서버는 직접 자식으로 띄운다(함수로 감싸면 신호가 감싼 subshell로 간다).
    # 비대화 bash의 백그라운드 자식은 SIGINT를 무시한 채 시작하고 bash는 그 신호를 다시 잡지 못하므로,
    # SIGINT에 끝나는 가짜는 처리기를 새로 거는 python3로 만든다(실제 서버는 tokio가 처리기를 건다).
    local fake_ok='import signal, sys, time
def stop(*_):
    sys.stderr.write("shutting down\n"); sys.exit(0)
signal.signal(signal.SIGINT, stop)
while True: time.sleep(0.1)'
    local fake_deaf='trap "" INT TERM; while :; do sleep 0.1; done'
    stop_ok() { (
        python3 -c "$fake_ok" >/dev/null 2>>"$t/fake.log" &
        SERVER_PID=$!
        sleep 0.3
        STOP_SECONDS=3 stop_server "$t/fake.log"
    ); }
    stop_deaf() { # 결과 줄이 실패로 남고 기한 근처에서 끝나며 가짜 서버가 남지 않아야 한다.
        local out start
        start=$SECONDS
        out=$( (
            bash -c "$fake_deaf" >/dev/null 2>&1 &
            SERVER_PID=$!
            echo "$SERVER_PID" >"$t/deaf.pid"
            sleep 0.3
            STOP_SECONDS=1 stop_server "$t/fake.log"
        ) 2>/dev/null)
        [[ "$(tail -n 1 <<<"$out")" == "rendezvous-check result=fail step=stop" ]] \
            && [[ $((SECONDS - start)) -le 5 ]] && gone "$(cat "$t/deaf.pid")"
    }
    cleanup_deaf() { # 실패 경로 정리가 무시하는 서버를 기한 안에 끝내고 거둔다.
        local start
        start=$SECONDS
        (
            bash -c "$fake_deaf" >/dev/null 2>&1 &
            SERVER_PID=$!
            echo "$SERVER_PID" >"$t/deaf.pid"
            TMP=""
            CLEANUP_SECONDS=1
            sleep 0.3
            cleanup 2>/dev/null
            [[ -z "$SERVER_PID" ]]
        ) && gone "$(cat "$t/deaf.pid")" && [[ $((SECONDS - start)) -le 5 ]]
    }
    # 시작 로그 기다리기. 가짜 서버가 시작 줄을 먼저 찍고 나머지를 늦게 찍는다.
    local late='echo "INFO s: rendezvous server started public_key=k primary=x" >&2; sleep 0.5
echo "INFO s: setting key=a value=1 source=configured" >&2; sleep 0.5
echo "WARN s: bound to a wildcard address" >&2; exec sleep 30.25'
    start_late() { # wait_start_log <예상 rc> <기한> <글자...>
        local want=$1 limit=$2 rc=0 pid reader left=0
        shift 2
        : >"$t/start.log"
        rm -f "${t:?}/out.fifo"
        mkfifo "$t/out.fifo"
        # 가짜의 표준 출력을 FIFO로 받는다. 출력을 쥔 프로세스가 모두 끝나야 읽는 쪽이 끝난다.
        cat "$t/out.fifo" >/dev/null &
        reader=$!
        bash -c "$late" 2>"$t/start.log" >"$t/out.fifo" &
        pid=$!
        wait_start_log "$t/start.log" "$pid" "$limit" "$@" || rc=$?
        # 마지막 sleep은 exec라 기록한 pid가 곧 sleep이다. 강제 종료 뒤 그 pid가 기한 안에 없어지고,
        # 출력을 쥔 고아 자식이 없어 읽는 쪽도 끝나는지 PID로 본다(다른 실행의 프로세스와 겹치지 않는다).
        # 서버 pid는 거두기(wait) 전에 기한을 두고 본다. 끝나지 않는 pid를 기한 없이 기다리지 않는다.
        sleep 0.3
        kill -s KILL "$pid" 2>/dev/null
        if wait_until 2 gone "$pid"; then
            wait_until 2 gone "$reader" || left=1
        else
            left=1
        fi
        kill -s KILL "$reader" 2>/dev/null
        wait "$reader" 2>/dev/null
        if gone "$pid"; then wait "$pid" 2>/dev/null; fi
        [[ "$rc" == "$want" && "$left" == 0 ]]
    }
    start_ends() { # 시작 줄만 찍고 끝나는 가짜: 2
        local rc=0 pid
        : >"$t/start.log"
        bash -c 'echo "rendezvous server started public_key=k " >&2; exit 1' 2>"$t/start.log" &
        pid=$!
        wait_start_log "$t/start.log" "$pid" 5 "rendezvous server started public_key=k " "wildcard address" || rc=$?
        wait "$pid" 2>/dev/null
        [[ "$rc" == 2 ]]
    }
    expect ok "wait_start_log: 늦게 찍히는 설정·경고 줄을 기한 안에서 기다린다" start_late 0 5 "rendezvous server started public_key=k " "setting key=a value=1 source=configured" "wildcard address"
    expect ok "wait_start_log: 끝내 안 나오는 줄은 기한 뒤 1" start_late 1 1.5 "rendezvous server started public_key=k " "setting key=b value=2 source=configured"
    expect ok "wait_start_log: 다 찍기 전에 끝나면 2" start_ends
    expect ok "missing_lines: 없는 줄만 낸다" test "$(printf 'a b\nc\n' >"$t/m.log"; missing_lines "$t/m.log" "a b" "d" "c")" == d

    expect ok "stop_server: SIGINT에 끝나는 서버는 통과" stop_ok
    expect ok "stop_server: 신호를 무시하는 서버는 기한 뒤 SIGKILL, result=fail step=stop" stop_deaf
    expect ok "cleanup: 신호를 무시하는 서버를 기한 뒤 SIGKILL로 거둔다" cleanup_deaf
    expect ok "wait_until: 기한 안에 성공" wait_until 1 true
    expect fail "wait_until: 기한을 넘기면 실패" wait_until 0.3 false
    slow_false() { sleep 0.4; false; }
    wait_slow() { # 부를 때마다 0.4 s 걸리는 명령도 기한 1 s 근처에서 끝난다(횟수로 세면 5 s 넘게 걸린다).
        local start
        start=$(now_us)
        ! wait_until 1 slow_false && [[ $(($(now_us) - start)) -lt 2000000 ]]
    }
    expect ok "wait_until: 기한을 경과 시각으로 잰다" wait_slow
    # 기한 1 s에 2.5 s 뒤 성공: 시계가 1초 해상도(%N 없는 date)여도 성공 시각이 끝 시각보다 1초 이상 뒤다.
    late_true() { sleep 2.5; true; }
    expect fail "wait_until: 기한 1 s에 2.5 s 뒤 성공하면 실패" wait_until 1 late_true
    expect ok "now_us: 늘어나는 마이크로초" test "$(now_us)" -gt 1000000000000000
    now_from_date() { local a b; a=$(now_us); b=$(unset EPOCHREALTIME; now_us); [[ "$b" -ge "$a" && $((b - a)) -lt 5000000 ]]; }
    expect ok "now_us: EPOCHREALTIME이 없으면 date로 같은 단위" now_from_date
    # 가짜 date를 PATH 앞에 둔다. +%s는 FAKE_EPOCH(없으면 지금 초)를, +%s%N은 그 뒤에 FAKE_NS를 붙여 낸다.
    # FAKE_NS=N은 %N을 모르는 date(BSD), 아홉 자리 숫자는 나노초를 내는 date(GNU)를 흉내 낸다.
    mkdir "$t/fake-date"
    cat >"$t/fake-date/date" <<'EOF'
#!/bin/sh
s=${FAKE_EPOCH:-$("$REAL_DATE" +%s)}
case "$1" in
+%s%N) echo "$s$FAKE_NS" ;;
+%s) echo "$s" ;;
esac
EOF
    chmod +x "$t/fake-date/date"
    with_fake_date() { # with_fake_date <FAKE_NS> <FAKE_EPOCH 또는 빈 값> <명령...>
        local PATH="$t/fake-date:$PATH" REAL_DATE FAKE_NS=$1 FAKE_EPOCH=$2
        REAL_DATE=$(PATH=${PATH#*:} command -v date)
        export REAL_DATE FAKE_NS FAKE_EPOCH
        [[ -n "$FAKE_EPOCH" ]] || unset FAKE_EPOCH
        shift 2
        unset EPOCHREALTIME
        "$@"
    }
    expect ok "now_us: %N을 모르는 date는 초를 마이크로초로 쓴다" test "$(with_fake_date N 1764230400 now_us)" == 1764230400000000
    expect ok "now_us: 숫자 나노초를 내는 date는 마이크로초로 자른다" test "$(with_fake_date 123456789 1764230400 now_us)" == 1764230400123456
    date_wait() { # %N을 모르는 date로도 기한 0.3 s의 실패 대기가 몇 초 안에 끝난다.
        local p
        (! with_fake_date N "" wait_until 0.3 false) &
        p=$!
        if wait_until 4 gone "$p"; then
            wait "$p"
        else
            kill -s KILL "$p" 2>/dev/null
            wait "$p" 2>/dev/null
            return 1
        fi
    }
    expect ok "wait_until: %N을 모르는 date로도 기한 뒤 끝난다" date_wait

    # 실제 예시 파일에도 판정 함수가 기대대로 걸리는지 본다(빌드 없이).
    expect ok "예시: 표 안에 적은 키가 있다" test -n "$(configured_keys "$EXAMPLE")"
    expect ok "예시: key_dir가 하나다" set_key_dir "$EXAMPLE" /tmp/k
    expect ok "예시: allowlist 묶음이 풀린다" grep -qx 'policy = "allowlist"' <(uncomment_access "$EXAMPLE" allowlist)
    expect ok "예시: everyone 묶음이 풀린다" grep -qx 'policy = "everyone"' <(uncomment_access "$EXAMPLE" everyone)
    expect ok "예시: 기본값 주석이 있다" test "$(configured_keys <(uncomment_defaults "$EXAMPLE") | wc -l)" -gt "$(configured_keys "$EXAMPLE" | wc -l)"

    if [[ "$fails" == 0 ]]; then
        echo "rendezvous-check self-test result=ok"
        return 0
    fi
    echo "rendezvous-check self-test result=fail failures=$fails"
    return 1
}

# ── 실제 검사 ────────────────────────────────────────────────────────

STEP=""
fail_at() {
    log "$1"
    echo "rendezvous-check result=fail step=$STEP"
    exit 1
}
step_ok() { echo "rendezvous-check step=$STEP ok${1:+ $1}"; }

build() {
    if [[ -n "${RENDEZVOUS_BIN:-}" || -n "${STUN_PROBE:-}" ]]; then
        [[ -x "${RENDEZVOUS_BIN:-}" && -x "${STUN_PROBE:-}" ]] \
            || { log "RENDEZVOUS_BIN과 STUN_PROBE는 둘 다 실행 파일이어야 한다"; exit 2; }
        BIN=$RENDEZVOUS_BIN
        PROBE=$STUN_PROBE
        return
    fi
    local json
    log "빌드: ${BUILD_CMD[*]}"
    json=$(cd "$ROOT" && "${BUILD_CMD[@]}") \
        || { log "빌드 실패"; echo "rendezvous-check result=fail step=build"; exit 2; }
    # 실행 파일 경로는 cargo 출력의 executable 값에서 얻는다(target 폴더 위치를 짐작하지 않는다).
    BIN=$(executable_of "$json" tasty-rendezvous)
    PROBE=$(executable_of "$json" stun_probe)
    [[ -x "$BIN" && -x "$PROBE" ]] \
        || { log "빌드 출력에서 실행 파일을 찾지 못했다: BIN=$BIN PROBE=$PROBE"; echo "rendezvous-check result=fail step=build"; exit 2; }
}

settings_of() { "$BIN" settings --config "$1" 2>&1; }

main_check() {
    local cfg out keyout pub opened acc probe_out addr target n
    build
    TMP=$(mktemp -d) || { log "임시 폴더를 만들지 못했다"; exit 2; }
    cfg=$TMP/rendezvous.toml

    STEP=keygen
    set_key_dir "$EXAMPLE" "$TMP/key" >"$cfg" || fail_at "예시의 key_dir를 바꾸지 못했다"
    keyout=$("$BIN" keygen --dir "$TMP/key" 2>&1) || fail_at "keygen 실패: $keyout"
    grep -qx 'server key created' <<<"$keyout" || fail_at "keygen이 새 키를 알리지 않았다: $keyout"
    pub=$(sed -n 's/^public key: \([0-9a-f]\{64\}\)$/\1/p' <<<"$keyout")
    [[ -n "$pub" ]] || fail_at "keygen 출력에 공개키가 없다: $keyout"
    keyout=$("$BIN" keygen --dir "$TMP/key" 2>&1) || fail_at "두 번째 keygen 실패: $keyout"
    if ! grep -qx 'existing server key kept' <<<"$keyout" || ! grep -qx "public key: $pub" <<<"$keyout"; then
        fail_at "두 번째 keygen이 같은 키를 두지 않았다: $keyout"
    fi
    step_ok

    STEP=settings
    out=$(settings_of "$cfg") || fail_at "settings --config 실패: $out"
    check_settings_format "$out" || fail_at "settings 출력 형식"
    check_configured "$out" "$cfg" || fail_at "settings 출력이 예시와 다르다"
    [[ "$(grep '^key_dir = ' <<<"$out")" == "key_dir = $TMP/key" ]] || fail_at "key_dir 줄: $(grep '^key_dir' <<<"$out")"
    [[ "$(grep '^access = ' <<<"$out")" == "access = everyone" ]] || fail_at "access 줄: $(grep '^access' <<<"$out")"
    n=$(grep -cE ' \((default|configured|fixed|computed)\)$' <<<"$out")
    step_ok "entries=$n configured=$(configured_keys "$cfg" | awk '{ printf "%s%s", s, $1; s = "," }')"

    STEP=defaults
    uncomment_defaults "$cfg" >"$TMP/defaults.toml"
    opened=$(settings_of "$TMP/defaults.toml") || fail_at "기본값 주석을 푼 사본을 읽지 못했다: $opened"
    check_defaults_match "$out" "$opened" "$TMP/defaults.toml" || fail_at "주석의 기본값이 실제 기본과 다르다"
    step_ok "keys=$(configured_keys "$TMP/defaults.toml" | wc -l | tr -d ' ')"

    STEP=access
    uncomment_access "$cfg" everyone >"$TMP/everyone.toml"
    acc=$(settings_of "$TMP/everyone.toml") || fail_at "everyone 예시를 읽지 못했다: $acc"
    [[ "$(grep '^access = ' <<<"$acc")" == "access = everyone" ]] || fail_at "everyone 예시의 access 줄: $(grep '^access' <<<"$acc")"
    uncomment_access "$cfg" allowlist >"$TMP/allowlist.toml"
    grep -qx 'policy = "allowlist"' "$TMP/allowlist.toml" || fail_at "allowlist 예시를 풀지 못했다"
    printf '# check-config.sh\n%s\n' "$pub" >"$TMP/allowlist.txt"
    acc=$(settings_of "$TMP/allowlist.toml") || fail_at "allowlist 예시를 읽지 못했다: $acc"
    [[ "$(grep '^access = ' <<<"$acc")" == "access = allowlist (1 devices, $TMP/allowlist.txt)" ]] \
        || fail_at "allowlist 예시의 access 줄: $(grep '^access' <<<"$acc")"
    step_ok "policies=everyone,allowlist"

    STEP=start
    "$BIN" --config "$cfg" 2>"$TMP/server.log" &
    SERVER_PID=$!
    # 시작 줄과 그 뒤의 설정 줄·wildcard 경고는 차례로 찍힌다. 모두 나올 때까지 기한 안에서 기다린다.
    local want=("rendezvous server started public_key=$pub ")
    while read -r key value; do
        want+=("setting key=$key value=$value source=configured")
    done < <(configured_keys "$cfg")
    want+=("wildcard address")
    local w=0
    wait_start_log "$TMP/server.log" "$SERVER_PID" "$START_SECONDS" "${want[@]}" || w=$?
    case "$w" in
        0) ;;
        2) cat "$TMP/server.log" >&2; fail_at "서버가 시작 로그를 다 찍기 전에 끝났다" ;;
        *) cat "$TMP/server.log" >&2; fail_at "시작 로그에 없는 줄(${START_SECONDS}s): $(missing_lines "$TMP/server.log" "${want[@]}")" ;;
    esac
    step_ok "pid=$SERVER_PID"

    for STEP in stun-primary stun-alternate; do
        case "$STEP" in
            stun-primary) addr=$(setting_of "$out" listen.primary) ;;
            stun-alternate)
                addr=$(setting_of "$out" listen.primary)
                addr="${addr%:*}:$(setting_of "$out" listen.alternate_port)"
                ;;
        esac
        addr=${addr% (*}
        target="$(probe_ip "$addr"):$(port_of "${addr% (*}")"
        probe_out=$("$PROBE" "$target" "$STUN_TIMEOUT_MS" 2>&1) || fail_at "STUN 응답 없음($target): $probe_out"
        check_probe_line "$probe_out" "$target" || fail_at "STUN 응답이 맞지 않다($target)"
        step_ok "$probe_out"
    done

    stop_server "$TMP/server.log"

    echo "rendezvous-check result=ok"
}

# stop 단계: SERVER_PID에 SIGINT를 보내 STOP_SECONDS 안에 종료 코드 0으로 끝나고 서버 로그 $1에
# 종료 줄이 있는지 본다. 기한을 넘기면 SIGKILL로 끝내고 거둔 뒤 실패한다.
stop_server() {
    STEP=stop
    if ! stop_process "$SERVER_PID" INT "$STOP_SECONDS"; then
        SERVER_PID=""
        fail_at "서버가 SIGINT 뒤 ${STOP_SECONDS}s 안에 끝나지 않아 SIGKILL로 끝냈다"
    fi
    SERVER_PID=""
    [[ "$STOP_RC" == 0 ]] || { cat "$1" >&2; fail_at "서버 종료 코드 $STOP_RC"; }
    grep -q 'shutting down' "$1" || fail_at "종료 로그가 없다"
    step_ok "rc=$STOP_RC"
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
