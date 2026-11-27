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
#   RENDEZVOUS_START_SECONDS       서버 시작 로그를 기다리는 시간. 기본 30
#   RENDEZVOUS_STUN_TIMEOUT_MS     STUN 응답 하나를 기다리는 시간. 기본 2000
set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"
EXAMPLE="$HERE/rendezvous.toml"
START_SECONDS="${RENDEZVOUS_START_SECONDS:-30}"
STUN_TIMEOUT_MS="${RENDEZVOUS_STUN_TIMEOUT_MS:-2000}"

log() { echo "[rendezvous-check] $*" >&2; }

# 파일 머리의 주석 블록(둘째 줄부터 첫 비주석 줄 전까지)을 도움말로 출력한다.
show_help() { awk 'NR > 1 && !/^#/ { exit } NR > 1 { sub(/^# ?/, ""); print }' "$0"; }

TMP=""
SERVER_PID=""
cleanup() {
    if [[ -n "$SERVER_PID" ]] && kill -0 "$SERVER_PID" 2>/dev/null; then
        kill "$SERVER_PID" 2>/dev/null
        wait "$SERVER_PID" 2>/dev/null
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
    log "빌드: cargo build --locked -p tasty-net-rendezvous --bin tasty-rendezvous --example stun_probe"
    json=$(cd "$ROOT" && cargo build --locked --message-format=json -p tasty-net-rendezvous \
        --bin tasty-rendezvous --example stun_probe) \
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
    local waited=0
    until grep -q 'rendezvous server started' "$TMP/server.log"; do
        kill -0 "$SERVER_PID" 2>/dev/null || { cat "$TMP/server.log" >&2; fail_at "서버가 시작 전에 끝났다"; }
        [[ "$waited" -lt $((START_SECONDS * 10)) ]] || { cat "$TMP/server.log" >&2; fail_at "서버 시작 로그를 ${START_SECONDS}s 안에 보지 못했다"; }
        sleep 0.1
        waited=$((waited + 1))
    done
    grep 'rendezvous server started' "$TMP/server.log" | grep -q "public_key=$pub" \
        || fail_at "시작 로그의 공개키가 keygen과 다르다: $(grep 'rendezvous server started' "$TMP/server.log")"
    while read -r key value; do
        grep -q "key=$key value=$value source=configured setting" "$TMP/server.log" \
            || grep -q "setting key=$key value=$value source=configured" "$TMP/server.log" \
            || fail_at "시작 로그에 설정 줄이 없다: $key=$value"
    done < <(configured_keys "$cfg")
    grep -q 'wildcard address' "$TMP/server.log" || fail_at "wildcard로 듣는데 경고 줄이 없다"
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

    STEP=stop
    kill -INT "$SERVER_PID"
    local rc=0
    wait "$SERVER_PID" || rc=$?
    SERVER_PID=""
    [[ "$rc" == 0 ]] || { cat "$TMP/server.log" >&2; fail_at "서버 종료 코드 $rc"; }
    grep -q 'shutting down' "$TMP/server.log" || fail_at "종료 로그가 없다"
    step_ok "rc=$rc"

    echo "rendezvous-check result=ok"
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
