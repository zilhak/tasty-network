#!/usr/bin/env bash
# relay 설정 예시 기동 검사. 동작 수준 기준 D5 (a)(.github/workflows/relay.yml의 relay-config-check 잡)가 부른다.
#
#   relay/check-config.sh [--keep-image] [설정.toml ...]
#   relay/check-config.sh --self-test     docker 없이 설정 읽기·변환·상한 요구 판정만 시험한다
#
# 설정을 주지 않으면 relay/punch-only.toml과 relay/relay-fallback.toml을 본다. 설정마다:
#   1. 운영 예시를 tomllib로 읽어 [tls] 표와 enable_quic_addr_discovery를 지운 값으로 CI용 설정을 임시 폴더에 새로 쓴다.
#   2. 고정 이미지(RELAY_IMAGE)를 --dev와 그 설정으로 띄운다. 호스트 쪽은 127.0.0.1의 빈 포트에 게시한다.
#   3. HTTP(/healthz, /, /generate_204)와 metrics(/metrics) 응답, 시작 로그의 평문 리스너 줄
#      ([HTTP/WS] relay: serving on), 시작 로그에 찍힌 수신 상한이 설정 파일과 같은지
#      (설정 파일을 실제로 읽었는지) 본다.
#   기본 예시 punch-only.toml은 수신 상한(client_rx)이 있어야, relay-fallback.toml은 없어야 통과한다.
#   설정은 python3 tomllib로 읽는다(3.11 이상).
#   4. 결과를 한 줄로 쓴다: relay-check config=<이름> result=ok|fail ...
# 띄운 컨테이너와 임시 폴더는 끝날 때 지운다. 이 스크립트가 받은 이미지도 지운다(--keep-image면 남긴다).
# TLS와 QUIC 주소 확인은 판정하지 않는다(docs/design/relay.md "배포 형태").
#
# 종료 코드: 0 모든 설정 통과, 1 하나라도 실패, 2 판정 불가(docker·curl·python3 없음, 인자 오류).
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

# 파일 $1 머리의 주석 블록(둘째 줄부터 첫 비주석 줄 전까지)을 도움말로 출력한다.
rc_help() { awk 'NR > 1 && !/^#/ { exit } NR > 1 { sub(/^# ?/, ""); print }' "$1"; }

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

# 컨테이너 $1(ID)을 지우고 없어졌는지 확인한다. 0 없어짐, 1 남음, 2 확인 불가(데몬 통신 오류 등).
# inspect 실패를 곧 부재로 보지 않는다. 오류 문구가 "No such container"일 때만 없어진 것으로 본다.
rc_remove_container() {
    local err
    docker rm -f "$1" >/dev/null 2>&1 || true
    if err=$(docker container inspect "$1" 2>&1 >/dev/null); then
        return 1
    fi
    if grep -qi 'no such container' <<<"$err"; then
        return 0
    fi
    rc_log "컨테이너 상태를 확인하지 못했다: $1 ($(head -1 <<<"$err"))"
    return 2
}

# docker create 인자로 컨테이너를 만든다. 성공하면 그 ID를 RC_NEW_ID에 두고 정리 목록에 넣는다.
# 실패하면 아무것도 등록하지 않는다(같은 이름의 남의 컨테이너가 있어 실패한 경우 그것을 지우지 않는다).
# 만든 뒤 start가 실패해도 ID가 목록에 있으므로 EXIT 정리가 지운다.
RC_NEW_ID=""
rc_create_container() {
    local id
    RC_NEW_ID=""
    id=$(docker create "$@") || return 1
    id=$(tail -1 <<<"$id")
    [[ -n "$id" ]] || return 1
    RC_CONTAINERS+=("$id")
    RC_NEW_ID="$id"
    return 0
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
    if ! python3 -I -c 'import tomllib' >/dev/null 2>&1; then
        rc_log "판정 불가: python3(3.11 이상, tomllib)이 없다."
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

# 운영 예시 $1을 tomllib로 읽어, 최상위 키 tls·enable_quic_addr_discovery를 지우고 접속 허용 비밀
# (shared_token 원소, http.bearer_token)을 시험값 RC_SECRET_PLACEHOLDER로, http.url을 자리표시 주소로
# 바꾼 값을 TOML로 다시 써서 CI용 설정 $2를 만든다. access는 허용 목록(아래 access_valid)에 든 모양만 받는다. 원본 텍스트를 고치지 않으므로
# 표 머리·점 키·인라인 표·인용 키("tls") 어느 표기로 적었든 파싱 결과의 같은 키가 지워진다.
# 검사 컨테이너는 debug 시작 로그에 ServerConfig 전체를 찍으므로 원래 토큰이 docker 로그 수집기에 닿지 않게 한다.
# 쓰는 값 종류: 표, 배열(표 원소는 인라인 표), 문자열, 정수, 유한한 실수, 불리언. 날짜·시각 값과 inf·nan은
# relay 설정에 쓰이지 않으므로 거절한다(rc 1). 다시 쓴 파일을 tomllib로 읽어 쓰려던 값과 같은지도 본다.
# 종료 코드: 0 만듦, 1 TOML 오류·지원하지 않는 값, 6 허용 목록 밖의 access 값. 0이 아니면 $2를 남기지 않는다.
RC_SECRET_PLACEHOLDER="check-config-placeholder"
rc_make_ci_config() {
    local src="$1" out="$2" rc
    rm -f "${out:?}"
    python3 -I - "$src" "$out" "$RC_SECRET_PLACEHOLDER" <<'PY'
import math, sys, tomllib

src, out, ph = sys.argv[1], sys.argv[2], sys.argv[3]

ACCESS_KINDS = ("everyone", "allowlist", "denylist", "http", "shared_token")
URL_PLACEHOLDER = "http://" + ph + ".invalid/"

def is_str_list(v, nonempty):
    return isinstance(v, list) and (v or not nonempty) and all(isinstance(x, str) and (x or not nonempty) for x in v)

def access_valid(d):
    # 허용 목록: access가 없음, 문자열 "everyone", 또는 키가 정확히 하나인 표(iroh-relay 1.5.0 main.rs AccessConfig).
    # 표의 키와 값 모양:
    #   everyone      빈 표 또는 빈 배열(toml 1.1.6 de/deserializer/table_enum.rs unit_variant가 받는 모양)
    #   allowlist     문자열 배열(endpoint ID)        denylist  같음
    #   http          표. url은 문자열(필수), bearer_token은 있으면 문자열, 다른 키 없음
    #   shared_token  비어 있지 않은 문자열만 있는 비어 있지 않은 배열(main.rs TryFrom<AccessConfig>)
    # 그 밖(배열, 오타 키, 키 두 개, 위치 기반 http 배열 표기 등)은 iroh가 받더라도 이 검사가 거절한다.
    # 어긋난 값은 출력하지 않는다(토큰 원문일 수 있다).
    if "access" not in d:
        return True
    acc = d["access"]
    if acc == "everyone":
        return True
    if not isinstance(acc, dict) or len(acc) != 1:
        return False
    (kind, v), = acc.items()
    if kind not in ACCESS_KINDS:
        return False
    if kind == "everyone":
        return v == {} or v == []
    if kind in ("allowlist", "denylist"):
        return is_str_list(v, nonempty=False)
    if kind == "shared_token":
        return is_str_list(v, nonempty=True)
    return (isinstance(v, dict) and isinstance(v.get("url"), str)
            and set(v) <= {"url", "bearer_token"}
            and isinstance(v.get("bearer_token", ""), str))

def scrub(d):
    # shared_token 원소와 bearer_token은 시험값으로, http.url은 통째로 자리표시 주소로 바꾼다
    # (쿼리·사용자 정보에 든 비밀이 시작 로그로 가지 않게. 콜백은 기동 검사에서 호출되지 않는다).
    acc = d.get("access")
    if isinstance(acc, dict):
        if "shared_token" in acc:
            acc["shared_token"] = [ph for _ in acc["shared_token"]]
        http = acc.get("http")
        if http is not None:
            http["url"] = URL_PLACEHOLDER
            if "bearer_token" in http:
                http["bearer_token"] = ph

class Unsupported(Exception):
    pass

def tstr(v):
    # TOML 기본 문자열. 큰따옴표와 역슬래시는 이스케이프, 제어 문자(U+0000~U+001F, U+007F)는 \uXXXX, 나머지(BMP 밖 포함)는 그대로 쓴다.
    out = []
    for ch in v:
        if ch == '"':
            out.append('\\"')
        elif ch == "\\":
            out.append("\\\\")
        elif ord(ch) < 0x20 or ord(ch) == 0x7F:
            out.append("\\u%04X" % ord(ch))
        else:
            out.append(ch)
    return '"' + "".join(out) + '"'

def val(v, path):
    # 값은 출력하지 않고 키 경로만 알린다(토큰 원문일 수 있다).
    if isinstance(v, bool):
        return "true" if v else "false"
    if isinstance(v, int):
        return str(v)
    if isinstance(v, float):
        if not math.isfinite(v):
            raise Unsupported(f"{path}: inf·nan 실수는 지원하지 않는다")
        return repr(v)
    if isinstance(v, str):
        return tstr(v)
    if isinstance(v, list):
        return "[" + ", ".join(val(x, f"{path}[{i}]") for i, x in enumerate(v)) + "]"
    if isinstance(v, dict):
        return "{" + ", ".join(f"{tstr(k)} = {val(x, path + '.' + k)}" for k, x in v.items()) + "}"
    raise Unsupported(f"{path}: {type(v).__name__} 값(날짜·시각 등)은 지원하지 않는다")

def dump(d, prefix, lines):
    for k, v in d.items():
        if not isinstance(v, dict):
            lines.append(f"{tstr(k)} = {val(v, '.'.join(prefix + [k]))}")
    for k, v in d.items():
        if isinstance(v, dict):
            name = prefix + [k]
            lines.append("")
            lines.append("[" + ".".join(tstr(x) for x in name) + "]")
            dump(v, name, lines)

try:
    with open(src, "rb") as f:
        a = tomllib.load(f)
except (OSError, tomllib.TOMLDecodeError) as e:
    print(e, file=sys.stderr)
    sys.exit(1)
if not access_valid(a):
    print("access-invalid: access must be absent, \"everyone\", or a table with one of everyone, allowlist, denylist, http, shared_token in the checked shape", file=sys.stderr)
    sys.exit(6)
a.pop("tls", None)
a.pop("enable_quic_addr_discovery", None)
scrub(a)
lines = ["# relay/check-config.sh가 만든 CI용 설정. TLS 표와 QUIC 주소 확인 키를 뺐고 접속 허용 비밀은 시험값으로 바꿨다."]
try:
    dump(a, [], lines)
except Unsupported as e:
    print(e, file=sys.stderr)
    sys.exit(1)
text = "\n".join(lines) + "\n"
if tomllib.loads(text) != a:
    print("CI용 설정을 다시 읽은 값이 쓰려던 값과 다르다", file=sys.stderr)
    sys.exit(1)
with open(out, "w", encoding="utf-8") as f:
    f.write(text)
PY
    rc=$?
    case $rc in
        0) return 0 ;;
        6) rc_log "$src: access 설정이 검사가 받는 표기가 아니다(relay/README.md 접속 허용. 값은 출력하지 않는다)." ;;
        *) rc_log "$src: CI용 설정을 만들지 못했다."; rc=1 ;;
    esac
    rm -f "${out:?}"
    return "$rc"
}

# TOML 파일 $1의 [limits.client.rx] 값을 "bytes_per_second max_burst_bytes"로 출력한다. 없는 값은 -.
# TOML 표기(주석, 1_000 같은 숫자, 인라인 표)를 iroh-relay와 같게 읽도록 파이썬 tomllib로 읽는다.
# 종료 코드: 0 읽음, 3 TOML 오류, 4 python3·tomllib 없음.
rc_toml_rx() {
    command -v python3 >/dev/null 2>&1 || return 4
    python3 -I - "$1" <<'PY'
import sys
try:
    import tomllib
except ImportError:
    sys.exit(4)
try:
    with open(sys.argv[1], "rb") as f:
        d = tomllib.load(f)
except (OSError, tomllib.TOMLDecodeError) as e:
    print(e, file=sys.stderr)
    sys.exit(3)
rx = d.get("limits", {}).get("client", {}).get("rx") if isinstance(d.get("limits"), dict) else None
if not isinstance(rx, dict):
    print("- -")
else:
    print(rx.get("bytes_per_second", "-"), rx.get("max_burst_bytes", "-"))
PY
}

# 설정 $1에 요구할 수신 상한: 기본 예시 punch-only.toml은 some, relay-fallback.toml은 none, 그 밖은 any.
rc_limit_expectation() {
    local d f
    d=$(cd "$(dirname "$1")" 2>/dev/null && pwd -P) || { echo any; return 0; }
    f="$d/$(basename "$1")"
    if [[ "$f" == "$(cd "$RELAY_DIR" && pwd -P)/punch-only.toml" ]]; then
        echo some
    elif [[ "$f" == "$(cd "$RELAY_DIR" && pwd -P)/relay-fallback.toml" ]]; then
        echo none
    else
        echo any
    fi
}

# 요구 $1(some|none|any)과 읽은 bytes_per_second $2(-는 없음)가 맞으면 0.
rc_limit_matches() {
    case "$1" in
        some) [[ "$2" != - ]] ;;
        none) [[ "$2" == - ]] ;;
        *) return 0 ;;
    esac
}

# TOML 파일 $1의 최상위 주소 키 $2(http_bind_addr 등)의 포트. 키가 없으면 기본 $3.
# tomllib로 읽으므로 작은따옴표 리터럴 문자열('[::]:8080')도 같게 읽는다. 포트를 못 읽으면 rc 1.
rc_toml_port() {
    python3 -I - "$1" "$2" "$3" <<'PY'
import sys, tomllib
with open(sys.argv[1], "rb") as f:
    v = tomllib.load(f).get(sys.argv[2])
if v is None:
    print(sys.argv[3])
    sys.exit(0)
port = v.rsplit(":", 1)[-1] if isinstance(v, str) and ":" in v else ""
if not (port.isascii() and port.isdigit()):
    sys.exit(1)
# 정수로 읽어 십진 표기로 돌려준다(iroh-relay의 SocketAddr처럼 "03340"은 3340). 게시·조회·리스너 비교가 이 값을 쓴다.
print(int(port))
PY
}

# 로그 $1에 --dev 평문 리스너 줄 "[HTTP/WS] relay: serving on <주소>:<포트 $2>"가 있으면 0.
# 주소는 설정의 http_bind_addr를 iroh-relay가 실제로 바인딩한 값이라(IPv4 0.0.0.0, IPv6 [::] 등) 보지 않는다
# (iroh-relay v1.5.0 server/http_server.rs:464-468, main.rs:597-599).
rc_listener_line_ok() {
    grep -Eq "\[HTTP/WS\] relay: serving on [^[:space:]]+:$2\$" <<<"$1"
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
    local bps burst logs flat want rx expect
    RC_LAST_CONTAINER="" RC_LAST_HTTP="" RC_LAST_METRICS="" RC_LAST_FAIL="" RC_LAST_LIMIT=""
    dir="$RC_TMP/$name"
    mkdir -p "$dir" || { RC_LAST_FAIL="stage=prepare"; return 1; }
    cfg="$dir/relay.toml"
    if [[ ! -f "$src" ]]; then
        # iroh-relay는 없는 설정 경로를 오류 없이 기본값으로 대신하므로 여기서 막는다.
        RC_LAST_FAIL="stage=config reason=missing-file"
        return 1
    fi
    rc_make_ci_config "$src" "$cfg"
    case $? in
        0) ;;
        6) RC_LAST_FAIL="stage=config reason=access-invalid"; return 1 ;;
        *) RC_LAST_FAIL="stage=config reason=ci-config"; return 1 ;;
    esac
    rx=$(rc_toml_rx "$cfg") || { RC_LAST_FAIL="stage=config reason=toml-rc$?"; return 1; }
    bps=${rx%% *}
    burst=${rx##* }
    expect=$(rc_limit_expectation "$src")
    if ! rc_limit_matches "$expect" "$bps"; then
        # 예: punch-only에서 [limits.client.rx] 표 이름을 틀리면 iroh-relay는 상한 없이 뜬다.
        RC_LAST_FAIL="stage=config reason=limit-expected-$expect bytes_per_second=$bps"
        return 1
    fi
    chmod 0644 "$cfg"
    http_port=$(rc_toml_port "$cfg" http_bind_addr 3340) || { RC_LAST_FAIL="stage=config reason=http_bind_addr"; return 1; }
    metrics_port=$(rc_toml_port "$cfg" metrics_bind_addr 9090) || { RC_LAST_FAIL="stage=config reason=metrics_bind_addr"; return 1; }

    # 환경 변수는 아래 두 개만 넘긴다. IROH_RELAY_HTTP_BEARER_TOKEN·IROH_RELAY_ACCESS_TOKEN은 넘기지 않는다
    # (넘기면 설정의 시험값 대신 실제 토큰이 쓰이고 시작 로그에 찍힌다).
    # 이름은 실행마다 고유하게 만든다(PID와 난수). create가 성공한 ID만 정리 목록에 들어간다.
    if ! rc_create_container \
        --name "relay-check-$name-$$-$RANDOM$RANDOM" \
        -e RUST_LOG=iroh_relay=debug -e NO_COLOR=1 \
        -v "$cfg:/config/relay.toml:ro" \
        -p "127.0.0.1::$http_port" -p "127.0.0.1::$metrics_port" \
        "$RELAY_IMAGE" --dev --config-path /config/relay.toml >/dev/null; then
        RC_LAST_FAIL="stage=start reason=docker-create"
        return 1
    fi
    cid="$RC_NEW_ID"
    RC_LAST_CONTAINER="$cid"
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
    logs=$(docker logs "$cid" 2>&1)
    flat=$(tr -d ' \t\n' <<<"$logs" | sed 's/,)/)/g; s/,}/}/g')
    if [[ "$bps" != - ]]; then
        if [[ "$burst" != - ]]; then
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
    if ! rc_listener_line_ok "$logs" "$http_port"; then
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
    RC_LAST_LIMIT="$bps"
    [[ "$bps" == - ]] && RC_LAST_LIMIT=none
    return 0
}

# 로그에서 큰따옴표 문자열을 모두 가린다. debug 시작 로그의 ServerConfig는 access의 shared_token 목록과
# http.bearer_token을 따옴표 문자열로 그대로 찍는다(iroh-relay v1.5.0 main.rs, Debug 구현).
rc_mask() { sed -E 's/"([^"\\]|\\.)*"/"<masked>"/g'; }

rc_dump_logs() {
    rc_log "컨테이너 로그(끝 40줄, 따옴표 문자열은 가림):"
    docker logs "$1" 2>&1 | tail -40 | rc_mask | sed 's/^/    /' >&2
}

# rc_start_relay가 띄운 컨테이너를 끝내고 지운다. 없어진 것이 확인될 때만 정리 목록에서 뺀다.
# 남으면 목록에 둬서 EXIT 정리가 다시 지운다.
rc_stop_relay() {
    local cid="$1" i
    [[ -n "$cid" ]] || return 0
    if ! rc_remove_container "$cid"; then
        rc_log "컨테이너가 남았거나 확인하지 못했다: $cid (끝날 때 다시 지운다)"
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

# docker 없이 설정 읽기·변환·상한 요구 판정을 시험한다. 통과 0, 실패 1, 판정 불가 2.
rc_self_test() {
    local t fail=0 got
    python3 -I -c 'import tomllib' >/dev/null 2>&1 || { rc_log "판정 불가: python3(tomllib)이 없다."; return 2; }
    t=$(mktemp -d) || return 2
    st_expect() { # 이름, 기대, 실제
        if [[ "$2" == "$3" ]]; then
            echo "self-test $1 ok"
        else
            echo "self-test $1 FAIL want=[$2] got=[$3]"
            fail=1
        fi
    }
    printf '[limits.client.rx] # 주석\nbytes_per_second = 16_384\nmax_burst_bytes = 65536\n' > "$t/comment.toml"
    st_expect rx-header-comment "16384 65536" "$(rc_toml_rx "$t/comment.toml")"
    printf '[limits.client.rxx]\nbytes_per_second = 100\n' > "$t/typo.toml"
    st_expect rx-typo-table "- -" "$(rc_toml_rx "$t/typo.toml")"
    printf 'limits.client.rx = { bytes_per_second = 7 }\n' > "$t/inline.toml"
    st_expect rx-inline "7 -" "$(rc_toml_rx "$t/inline.toml")"
    got=$(printf '%s\n' 'access: SharedTokenAccess(' '    "tok\"en-a",' 'bearer_token: Some("b-1"),' | rc_mask | tr '\n' ' ')
    st_expect mask-tokens 'access: SharedTokenAccess(     "<masked>", bearer_token: Some("<masked>"), ' "$got"
    rc_listener_line_ok '2026-01-01T00:00:00Z  INFO iroh_relay::server::http_server: [HTTP/WS] relay: serving on 0.0.0.0:3340' 3340
    st_expect listener-ipv4 0 "$?"
    rc_listener_line_ok '2026-01-01T00:00:00Z  INFO iroh_relay::server::http_server: [HTTP/WS] relay: serving on [::]:3340' 3340
    st_expect listener-ipv6 0 "$?"
    rc_listener_line_ok '2026-01-01T00:00:00Z  INFO iroh_relay::server::http_server: [HTTP/WS] relay: serving on 0.0.0.0:8080' 3340
    st_expect listener-other-port-rejected 1 "$?"
    rc_listener_line_ok '2026-01-01T00:00:00Z  INFO iroh_relay::server::http_server: [HTTP/WS] relay: serving on 0.0.0.0:33400' 3340
    st_expect listener-longer-port-rejected 1 "$?"
    printf "http_bind_addr = '[::]:8080'\n" > "$t/literal.toml"
    st_expect port-literal-string 8080 "$(rc_toml_port "$t/literal.toml" http_bind_addr 3340)"
    st_expect port-default 9090 "$(rc_toml_port "$t/literal.toml" metrics_bind_addr 9090)"
    printf 'http_bind_addr = "0.0.0.0:03340"\n' > "$t/zero.toml"
    got=$(rc_toml_port "$t/zero.toml" http_bind_addr 3340)
    st_expect port-leading-zero 3340 "$got"
    rc_listener_line_ok '2026-01-01T00:00:00Z  INFO iroh_relay::server::http_server: [HTTP/WS] relay: serving on 0.0.0.0:3340' "$got"
    st_expect port-leading-zero-listener 0 "$?"
    # 정리 소유권: docker를 흉내 내는 함수로 응답을 바꿔 본다(서브셸 안에서만).
    (
        docker() { if [[ $1 == create ]]; then echo 'Error: Conflict. The container name is already in use' >&2; return 1; fi; return 0; }
        RC_CONTAINERS=()
        rc_create_container --name taken img >/dev/null 2>&1
        echo "$? ${#RC_CONTAINERS[@]}"
    ) > "$t/own"
    st_expect create-fail-not-registered "1 0" "$(cat "$t/own")"
    (
        docker() { if [[ $1 == create ]]; then echo 0123abcd; fi; return 0; }
        RC_CONTAINERS=()
        rc_create_container --name mine img >/dev/null 2>&1
        echo "$? ${RC_CONTAINERS[*]}"
    ) > "$t/own"
    st_expect create-ok-registered "0 0123abcd" "$(cat "$t/own")"
    (
        docker() { if [[ $1 == container ]]; then echo 'Error: No such container: x' >&2; return 1; fi; return 0; }
        rc_remove_container x
    )
    st_expect remove-gone 0 "$?"
    (
        docker() { if [[ $1 == container ]]; then echo 'Cannot connect to the Docker daemon at unix:///var/run/docker.sock.' >&2; return 1; fi; return 1; }
        rc_remove_container x 2>/dev/null
    )
    st_expect remove-daemon-error-unknown 2 "$?"
    (
        docker() { return 0; }
        rc_remove_container x
    )
    st_expect remove-still-there 1 "$?"
    (
        docker() { if [[ $1 == container ]]; then echo 'Cannot connect to the Docker daemon' >&2; return 1; fi; return 1; }
        RC_CONTAINERS=(x)
        rc_stop_relay x 2>/dev/null
        echo "$? ${RC_CONTAINERS[*]}"
    ) > "$t/own"
    st_expect stop-keeps-unconfirmed "1 x" "$(cat "$t/own")"
    # 접속 허용 비밀은 CI용 설정에서 시험값으로 바뀌고 원래 문자열이 남지 않는다.
    printf 'enable_relay = true\naccess.shared_token = ["n10-secret-a", "n10-secret-b"]\n[limits.client.rx]\nbytes_per_second = 9\n' > "$t/shared.toml"
    rc_make_ci_config "$t/shared.toml" "$t/ci-shared.toml"
    st_expect secret-shared-converted 0 "$?"
    grep -q 'n10-secret' "$t/ci-shared.toml"
    st_expect secret-shared-absent 1 "$?"
    st_expect secret-shared-placeholder 2 "$(grep -o "$RC_SECRET_PLACEHOLDER" "$t/ci-shared.toml" | wc -l | tr -d ' ')"
    st_expect secret-shared-limit-kept "9 -" "$(rc_toml_rx "$t/ci-shared.toml")"
    printf '[access.http]\nurl = "https://auth.example.com/x"\nbearer_token = "n10-secret-c"\n' > "$t/bearer.toml"
    rc_make_ci_config "$t/bearer.toml" "$t/ci-bearer.toml"
    st_expect secret-bearer-converted 0 "$?"
    grep -q 'n10-secret' "$t/ci-bearer.toml"
    st_expect secret-bearer-absent 1 "$?"
    # http.url은 통째로 자리표시 주소로 바뀐다(쿼리·사용자 정보에 든 비밀도 남지 않는다).
    st_expect secret-bearer-url-replaced "http://$RC_SECRET_PLACEHOLDER.invalid/" "$(python3 -I -c 'import sys, tomllib
print(tomllib.load(open(sys.argv[1], "rb"))["access"]["http"]["url"])' "$t/ci-bearer.toml")"
    printf '[access.http]\nurl = "https://n10-user:n10-pass@auth.example.com/x?key=n10-query-secret"\n' > "$t/query.toml"
    rc_make_ci_config "$t/query.toml" "$t/ci-query.toml"
    st_expect secret-url-query-converted 0 "$?"
    grep -q 'n10-' "$t/ci-query.toml"
    st_expect secret-url-query-absent 1 "$?"
    grep -q "$RC_SECRET_PLACEHOLDER.invalid" "$t/ci-query.toml"
    st_expect secret-url-query-placeholder 0 "$?"
    # 허용 목록 안의 access 표기는 받는다.
    local ok n=0
    for ok in 'access = "everyone"' 'access.everyone = {}' 'access = { everyone = [] }' \
        'access.allowlist = ["ae58ff8833241ac82d6ff7611046ed67b5072d142c588d0063e942d9a75502b6"]' 'access.denylist = []'; do
        n=$((n + 1))
        printf 'enable_relay = true\n%s\n' "$ok" > "$t/acc-ok$n.toml"
        rc_make_ci_config "$t/acc-ok$n.toml" "$t/ci-acc-ok$n.toml"
        st_expect "access-valid-$n" 0 "$?"
    done
    # 허용 목록 밖의 access 값은 치환하지 않고 access-invalid로 끝나며, 원래 값을 출력하지 않는다.
    # 1~5: iroh-relay가 기동을 거절하는 값. 6~: 오타 키 3종, 배열, 키 두 개, 위치 기반 http 표기, http의 다른 키·url 없음, everyone에 값.
    local bad
    n=0
    for bad in 'access.shared_token = []' 'access.shared_token = [""]' 'access.shared_token = [4242]' \
        "access.http = { url = \"https://auth.example.com/x\", bearer_token = 4243 }" \
        "access.shared_token = 'n10-scalar-secret'" \
        'access.sharedtoken = ["n10-typo-a"]' 'access.shared-token = ["n10-typo-b"]' 'access.Shared_Token = ["n10-typo-c"]' \
        'access = ["n10-array"]' 'access = { shared_token = ["n10-two"], allowlist = [] }' \
        'access.http = ["https://auth.example.com/n10-pos", "n10-pos-token"]' \
        'access.http = { url = "https://auth.example.com/x", bearer_token = "n10-extra", timeout = 5 }' \
        'access.http = { bearer_token = "n10-no-url" }' 'access.everyone = { n10-key = "n10-everyone" }'; do
        n=$((n + 1))
        printf 'enable_relay = true\n%s\n' "$bad" > "$t/acc$n.toml"
        rc_make_ci_config "$t/acc$n.toml" "$t/ci-acc$n.toml" > "$t/acc$n.out" 2>&1
        st_expect "access-invalid-$n" 6 "$?"
        grep -Eq '4242|4243|n10-' "$t/acc$n.out"
        st_expect "access-invalid-$n-value-hidden" 1 "$?"
        st_expect "access-invalid-$n-ci-file-removed" no "$(if [[ -e "$t/ci-acc$n.toml" ]]; then echo yes; else echo no; fi)"
    done
    # rc_start_relay에서도 컨테이너를 만들기 전에 access-invalid로 끝난다(docker를 부르지 않는다).
    (
        docker() { echo called >> "$t/docker-calls"; return 1; }
        RC_TMP="$t"
        rc_start_relay acc5 "$t/acc5.toml" > "$t/start.out" 2>&1
        echo "$? $RC_LAST_FAIL"
    ) > "$t/start.res"
    st_expect access-invalid-start "1 stage=config reason=access-invalid" "$(cat "$t/start.res")"
    st_expect access-invalid-no-docker no "$(if [[ -e "$t/docker-calls" ]]; then echo yes; else echo no; fi)"
    grep -q 'n10-scalar-secret' "$t/start.out"
    st_expect access-invalid-start-value-hidden 1 "$?"
    # 다시 쓴 CI용 설정이 BMP 밖 문자·이스케이프·제어 문자를 TOML 규칙대로 보존한다(url은 바뀌므로 allowlist 원소로 본다).
    printf 'access.allowlist = ["https://example.org/auth/\xf0\x9f\x94\x91?q=\\"x\\"\\\\y\\t"]\n' > "$t/emoji.toml"
    rc_make_ci_config "$t/emoji.toml" "$t/ci-emoji.toml"
    st_expect encoder-emoji-converted 0 "$?"
    python3 -I -c 'import sys, tomllib
a = tomllib.load(open(sys.argv[1], "rb"))["access"]["allowlist"][0]
b = tomllib.load(open(sys.argv[2], "rb"))["access"]["allowlist"][0]
sys.exit(0 if a == b and "\U0001F511" in b and "\t" in b else 1)' "$t/emoji.toml" "$t/ci-emoji.toml"
    st_expect encoder-emoji-kept 0 "$?"
    printf '[limits\n' > "$t/bad.toml"
    rc_toml_rx "$t/bad.toml" >/dev/null 2>&1
    st_expect toml-error-rc 3 "$?"
    # 기본 예시와 같은 자리의 punch-only.toml에서 표 이름을 rxx로 틀리면 상한 요구에서 실패해야 한다.
    sed 's/^\[limits\.client\.rx\]/[limits.client.rxx]/' "$RELAY_DIR/punch-only.toml" > "$t/typo-punch.toml"
    got=$(rc_toml_rx "$t/typo-punch.toml")
    rc_limit_matches "$(rc_limit_expectation "$RELAY_DIR/punch-only.toml")" "${got%% *}"
    st_expect punch-only-typo-rejected 1 "$?"
    got=$(rc_toml_rx "$RELAY_DIR/punch-only.toml")
    rc_limit_matches "$(rc_limit_expectation "$RELAY_DIR/punch-only.toml")" "${got%% *}"
    st_expect punch-only-limit-required 0 "$?"
    got=$(rc_toml_rx "$RELAY_DIR/relay-fallback.toml")
    rc_limit_matches "$(rc_limit_expectation "$RELAY_DIR/relay-fallback.toml")" "${got%% *}"
    st_expect relay-fallback-no-limit 0 "$?"
    rc_limit_matches "$(rc_limit_expectation "$RELAY_DIR/relay-fallback.toml")" 100
    st_expect relay-fallback-limit-rejected 1 "$?"
    st_expect other-file-any any "$(rc_limit_expectation "$t/typo.toml")"
    rc_make_ci_config "$RELAY_DIR/punch-only.toml" "$t/ci-punch.toml"
    st_expect ci-config-punch-only 0 "$?"
    st_expect ci-config-punch-only-rx "16384 65536" "$(rc_toml_rx "$t/ci-punch.toml")"
    rc_make_ci_config "$RELAY_DIR/relay-fallback.toml" "$t/ci-fallback.toml"
    st_expect ci-config-relay-fallback 0 "$?"
    # CI용 설정은 파싱한 값에서 다시 만들므로 점 키·인용 키로 적은 tls·enable_quic_addr_discovery도 지워진다.
    printf 'tls.cert_mode = "Manual"\nenable_relay = true\n' > "$t/dotted-tls.toml"
    rc_make_ci_config "$t/dotted-tls.toml" "$t/out.toml" 2>/dev/null
    st_no_tls() { python3 -I -c 'import sys, tomllib
d = tomllib.load(open(sys.argv[1], "rb"))
sys.exit(0 if "tls" not in d and "enable_quic_addr_discovery" not in d else 1)' "$1"; }
    st_expect ci-config-dotted-tls-removed "0 0" "$? $(st_no_tls "$t/out.toml"; echo $?)"
    local base
    for base in punch-only relay-fallback; do
        sed -e 's/^\[tls\]/["tls"]/' -e 's/^enable_quic_addr_discovery =/"enable_quic_addr_discovery" =/' \
            "$RELAY_DIR/$base.toml" > "$t/quoted-$base.toml"
        grep -Eq '^\["tls"\]' "$t/quoted-$base.toml" && grep -q '^"enable_quic_addr_discovery" =' "$t/quoted-$base.toml"
        st_expect "ci-config-quoted-$base-variant-made" 0 "$?"
        rc_make_ci_config "$t/quoted-$base.toml" "$t/ci-quoted-$base.toml"
        st_expect "ci-config-quoted-$base" 0 "$?"
        rc_make_ci_config "$RELAY_DIR/$base.toml" "$t/ci-plain-$base.toml"
        cmp -s "$t/ci-quoted-$base.toml" "$t/ci-plain-$base.toml"
        st_expect "ci-config-quoted-$base-same-as-default" 0 "$?"
    done
    st_no_tls "$t/ci-punch.toml"
    st_expect ci-config-punch-only-no-tls 0 "$?"
    printf 'enable_relay = true\nwhen = 2026-01-01T00:00:00Z\n' > "$t/datetime.toml"
    rc_make_ci_config "$t/datetime.toml" "$t/ci-datetime.toml" 2>/dev/null
    st_expect ci-config-datetime-rejected "1 no" "$? $(if [[ -e "$t/ci-datetime.toml" ]]; then echo yes; else echo no; fi)"
    printf 'enable_relay = true\nratio = inf\n' > "$t/inf.toml"
    rc_make_ci_config "$t/inf.toml" "$t/ci-inf.toml" 2>/dev/null
    st_expect ci-config-inf-rejected 1 "$?"
    printf 'enable_relay = true\nf = 1.5e-3\nlist = [1, "a", [true], { k = 2 }]\n[t.\"q k\"]\nv = -7\n' > "$t/kinds.toml"
    rc_make_ci_config "$t/kinds.toml" "$t/ci-kinds.toml"
    st_expect ci-config-value-kinds 0 "$?"
    python3 -I -c 'import sys, tomllib
sys.exit(0 if tomllib.load(open(sys.argv[1], "rb")) == tomllib.load(open(sys.argv[2], "rb")) else 1)' "$t/kinds.toml" "$t/ci-kinds.toml"
    st_expect ci-config-value-kinds-same 0 "$?"
    rm -rf "${t:?}"
    return "$fail"
}

rc_main() {
    local configs=() src name failed=0 prep
    while [[ $# -gt 0 ]]; do
        case "$1" in
            --keep-image) rc_keep_image ;;
            --self-test)
                rc_self_test
                return $?
                ;;
            -h | --help)
                rc_help "${BASH_SOURCE[0]}"
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
