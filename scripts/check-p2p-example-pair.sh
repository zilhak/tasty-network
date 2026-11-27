#!/usr/bin/env bash
# P2P 예제 두 프로세스 검사(docs/design/testing.md "크레이트 단독", 동작 수준 기준 D1).
# rendezvous 서버 실행 파일(tasty-rendezvous)과 예제 p2p_peer를 빌드하고, 루프백에 서버 하나와
# 예제 두 개(--listen, --dial)를 띄워 펀칭·연결·echo를 확인한 뒤 셋 모두 끝났는지 본다.
# 실행 파일 경로는 cargo build --message-format=json 출력의 executable 값에서 얻는다
# (OS마다 다른 경로를 짐작하지 않는다).
#
# 사용: bash scripts/check-p2p-example-pair.sh
# 종료 코드: 통과 0, 실패 1, 판정 불가(도구 없음·빌드 실패·실행 파일 경로 없음) 2.

set -euo pipefail

cd "$(dirname "$0")/.."

# 예제 한 번의 상한(초). 예제 --timeout으로 넘긴다.
PEER_TIMEOUT=60
# 서버가 시작 줄을 쓸 때까지 기다리는 상한(초)과 서버 정지를 기다리는 상한(초).
SERVER_START_LIMIT=30
SERVER_STOP_LIMIT=15
# 위 두 대기의 확인 간격(초).
POLL=0.2

PY=""
for c in python3 python; do
    if command -v "$c" >/dev/null 2>&1; then PY="$c"; break; fi
done
[ -n "$PY" ] || { echo "[p2p-example-pair] 판정 불가: python3가 PATH에 없다." >&2; exit 2; }

TMP="$(mktemp -d)"
PIDS=()
cleanup() {
    local p
    for p in "${PIDS[@]}"; do
        kill "$p" 2>/dev/null || true
    done
    rm -rf "${TMP:?}"
}
trap cleanup EXIT

fail() {
    echo "[p2p-example-pair] 실패: $*" >&2
    local f
    for f in "$TMP"/*.log; do
        [ -f "$f" ] || continue
        echo "--- $(basename "$f")" >&2
        cat "$f" >&2
    done
    exit 1
}

# cargo build --message-format=json 출력에서 대상 이름 $1, 종류 $2의 executable 경로를 꺼낸다.
# Windows 경로의 역슬래시는 Git Bash가 받는 슬래시로 바꾼다.
executable() {
    "$PY" -c '
import json, sys
name, kind = sys.argv[1], sys.argv[2]
for line in sys.stdin:
    try:
        m = json.loads(line)
    except ValueError:
        continue
    t = m.get("target") or {}
    if m.get("reason") == "compiler-artifact" and t.get("name") == name and kind in t.get("kind", []) and m.get("executable"):
        print(m["executable"].replace("\\", "/"))
' "$1" "$2"
}

build() {
    local out
    echo "+ cargo build --locked $* --message-format=json" >&2
    out=$(cargo build --locked "$@" --message-format=json) || {
        echo "[p2p-example-pair] 판정 불가: cargo build $*가 실패했다." >&2
        exit 2
    }
    printf '%s\n' "$out"
}

SERVER=$(build -p tasty-net-rendezvous --bin tasty-rendezvous | executable tasty-rendezvous bin)
PEER=$(build -p tasty-net-p2p --example p2p_peer | executable p2p_peer example)
if [ -z "$SERVER" ] || [ ! -x "$SERVER" ]; then
    echo "[p2p-example-pair] 판정 불가: tasty-rendezvous 실행 파일 경로를 얻지 못했다." >&2
    exit 2
fi
if [ -z "$PEER" ] || [ ! -x "$PEER" ]; then
    echo "[p2p-example-pair] 판정 불가: p2p_peer 실행 파일 경로를 얻지 못했다." >&2
    exit 2
fi

# 서버 키와 설정. 포트는 0으로 두고 서버가 쓰는 시작 줄에서 실제 주소를 읽는다.
"$SERVER" keygen --dir "$TMP/server-key" > "$TMP/keygen.log" 2>&1 || fail "서버 keygen"
SERVER_KEY=$(sed -n 's/^public key: //p' "$TMP/keygen.log" | tr -d '\r')
[ -n "$SERVER_KEY" ] || fail "keygen 출력에 공개 키가 없다"
cat > "$TMP/server.toml" <<'EOF'
key_dir = "server-key"

[listen]
primary = "127.0.0.1:0"
alternate_port = 0
EOF

"$SERVER" --config "$TMP/server.toml" > /dev/null 2> "$TMP/server.log" &
SERVER_PID=$!
PIDS+=("$SERVER_PID")
ADDR=""
deadline=$((SECONDS + SERVER_START_LIMIT))
while [ -z "$ADDR" ] && [ "$SECONDS" -lt "$deadline" ]; do
    kill -0 "$SERVER_PID" 2>/dev/null || fail "서버가 시작하다 끝났다"
    ADDR=$(sed -n 's/.*rendezvous server started.* primary=\([^ ]*\).*/\1/p; s/.* primary=\([^ ]*\).*rendezvous server started.*/\1/p' "$TMP/server.log" | head -n 1 | tr -d '\r')
    [ -n "$ADDR" ] || sleep "$POLL"
done
[ -n "$ADDR" ] || fail "서버 시작 줄이 ${SERVER_START_LIMIT}초 안에 나오지 않았다"
echo "[p2p-example-pair] 서버 $ADDR" >&2

LISTENER_ID=$("$PEER" id --key-dir "$TMP/listener" | tr -d '\r') || fail "listener 키"
DIALER_ID=$("$PEER" id --key-dir "$TMP/dialer" | tr -d '\r') || fail "dialer 키"
COMMON=(--server "$ADDR" --server-key "$SERVER_KEY" --timeout "$PEER_TIMEOUT")

"$PEER" --listen --key-dir "$TMP/listener" --peer "$DIALER_ID" "${COMMON[@]}" \
    > "$TMP/listener.out" 2> "$TMP/listener.log" &
LISTENER_PID=$!
PIDS+=("$LISTENER_PID")
"$PEER" --dial "$LISTENER_ID" --key-dir "$TMP/dialer" "${COMMON[@]}" \
    > "$TMP/dialer.out" 2> "$TMP/dialer.log" &
DIALER_PID=$!
PIDS+=("$DIALER_PID")

dial_rc=0
wait "$DIALER_PID" || dial_rc=$?
listen_rc=0
wait "$LISTENER_PID" || listen_rc=$?
cat "$TMP/dialer.out" "$TMP/listener.out" >&2
[ "$dial_rc" -eq 0 ] || fail "dialer 종료 코드 $dial_rc"
[ "$listen_rc" -eq 0 ] || fail "listener 종료 코드 $listen_rc"
grep -q '^echo ok ' "$TMP/dialer.out" || fail "dialer가 echo를 확인하지 못했다"
grep -q '^echoed ' "$TMP/listener.out" || fail "listener가 echo하지 못했다"

# 서버는 Ctrl-C(SIGINT)로 멈춘다. Windows의 Git Bash는 네이티브 프로세스에 SIGINT를 전하지
# 못하므로 그곳에서는 종료만 확인한다.
case "$(uname -s)" in
    MINGW* | MSYS* | CYGWIN*) kill "$SERVER_PID" 2>/dev/null || true ;;
    *) kill -INT "$SERVER_PID" 2>/dev/null || true ;;
esac
deadline=$((SECONDS + SERVER_STOP_LIMIT))
while kill -0 "$SERVER_PID" 2>/dev/null && [ "$SECONDS" -lt "$deadline" ]; do
    sleep "$POLL"
done
kill -0 "$SERVER_PID" 2>/dev/null && fail "서버가 ${SERVER_STOP_LIMIT}초 안에 멈추지 않았다"
server_rc=0
wait "$SERVER_PID" || server_rc=$?
case "$(uname -s)" in
    MINGW* | MSYS* | CYGWIN*) ;;
    *) [ "$server_rc" -eq 0 ] || fail "서버 종료 코드 $server_rc" ;;
esac
PIDS=()
echo "[p2p-example-pair] 통과: 서버 하나와 예제 두 개가 펀칭·연결·echo 뒤 모두 끝났다." >&2
