#!/usr/bin/env bash
# P2P 크레이트 격리 검사(docs/design/crates.md "P2P 크레이트 격리", 동작 수준 기준 D8).
# tasty-net-p2p·tasty-net-rendezvous가 이 workspace 안에서 의존하는 크레이트가 허용 범위 안인지
# cargo metadata의 resolve 그래프(package ID)로 본다. 대상의 일반·build·dev 의존을 보고, 그
# 뒤로 일반·build 의존을 따라가 닿는 workspace 크레이트도 본다(workspace 밖 크레이트를 거쳐도).
#   허용: tasty-net-rendezvous → tasty-net-identity
#         tasty-net-p2p        → tasty-net-identity, tasty-net-rendezvous
#
# 사용: bash scripts/check-p2p-isolation.sh
#       bash scripts/check-p2p-isolation.sh --metadata <파일>   cargo metadata 출력 JSON 파일로 판정한다
#       bash scripts/check-p2p-isolation.sh --self-test         판정 함수를 고정 사례로 시험한다
# 종료 코드: 통과 0, 위반(또는 자체 시험 실패) 1, 판정 불가(도구 없음·metadata 실패·대상 크레이트 0개·
# resolve 그래프가 없거나 대상 크레이트의 노드가 그래프에 없음) 2.

set -euo pipefail

cd "$(dirname "$0")/.."

PY=""
for c in python3 python; do
    if command -v "$c" >/dev/null 2>&1; then PY="$c"; break; fi
done
[ -n "$PY" ] || { echo "[p2p-isolation] 판정 불가: python3가 PATH에 없다." >&2; exit 2; }

TMP="$(mktemp -d)"
trap 'rm -rf "${TMP:?}"' EXIT

MODE=check
case "${1:-}" in
    --self-test)
        MODE=self-test
        ;;
    --metadata)
        [ -f "${2:-}" ] || { echo "[p2p-isolation] 판정 불가: metadata 파일이 없다: ${2:-}" >&2; exit 2; }
        cp "$2" "$TMP/metadata.json"
        ;;
    "")
        # 선택 의존과 모든 target의 의존이 들어가도록 --all-features, --filter-platform 없이 읽는다.
        cargo metadata --format-version 1 --locked --all-features > "$TMP/metadata.json" || {
            echo "[p2p-isolation] 판정 불가: cargo metadata가 실패했다." >&2
            exit 2
        }
        ;;
    *)
        echo "[p2p-isolation] 모르는 인자: $*" >&2
        exit 2
        ;;
esac

"$PY" - "$MODE" "$TMP" <<'PY'
import json
import os
import sys

# 대상 크레이트와 그 크레이트가 이 workspace 안에서 의존해도 되는 크레이트(crates.md "의존 방향").
ALLOWED = {
    "tasty-net-rendezvous": {"tasty-net-identity"},
    "tasty-net-p2p": {"tasty-net-identity", "tasty-net-rendezvous"},
}


class Undecidable(Exception):
    pass


def judge(meta):
    """(위반 목록, 대상 수, 요약). 대상 크레이트가 0개면 대상 수 0. resolve 그래프가 없거나
    (cargo metadata --no-deps 출력 등) 대상 크레이트의 노드가 그래프에 없으면 Undecidable.

    workspace 소속은 이름이 아니라 package ID(workspace_members)로 정하고, 의존은 resolve
    그래프의 package ID로 따라간다. 그래서 workspace 크레이트와 이름이 같은 다른 출처의
    크레이트는 workspace 의존이 아니고, 이름을 바꿔 쓴 workspace 의존이나 workspace 밖
    크레이트를 거쳐 닿는 workspace 크레이트도 잡는다."""
    members = set(meta.get("workspace_members", []))
    name_of = {p["id"]: p["name"] for p in meta.get("packages", [])}
    resolve = meta.get("resolve")
    if not isinstance(resolve, dict) or not isinstance(resolve.get("nodes"), list):
        raise Undecidable("metadata에 resolve 그래프가 없다(--no-deps 출력은 판정할 수 없다)")
    nodes = {n["id"]: n for n in resolve["nodes"]}

    def edges(pid):
        out = []
        for d in nodes.get(pid, {}).get("deps", []):
            kinds = {k.get("kind") or "normal" for k in d.get("dep_kinds", [])} or {"normal"}
            out.append((d["pkg"], kinds))
        return out

    violations = []
    by_name = {name_of[m]: m for m in members if m in name_of}
    targets = sorted(n for n in ALLOWED if n in by_name)
    missing = [t for t in targets if by_name[t] not in nodes]
    if missing:
        raise Undecidable(f"대상 크레이트의 노드가 resolve 그래프에 없다: {', '.join(missing)}")
    for t in targets:
        tid = by_name[t]
        allowed = ALLOWED[t]
        # 대상의 직접 의존은 일반·build·dev, 그 뒤는 일반·build 의존만 따라간다.
        stack = [(dep, [name_of.get(dep, dep)], sorted(kinds))
                 for dep, kinds in edges(tid) if kinds & {"normal", "build", "dev"}]
        seen = set()
        while stack:
            pid, path, kinds = stack.pop()
            if pid in seen:
                continue
            seen.add(pid)
            how = "/".join(kinds) + (" 의존" if len(path) == 1 else " 의존을 거침")
            if pid == tid:
                violations.append(f"{t} → {' → '.join(path)} (순환)")
                continue
            if pid in members and name_of.get(pid) not in allowed:
                violations.append(f"{t} → {' → '.join(path)} ({how})")
                continue
            for dep, k in edges(pid):
                if k & {"normal", "build"}:
                    stack.append((dep, path + [name_of.get(dep, dep)], sorted(k & {"normal", "build"})))
    summary = (f"대상 크레이트 {len(targets)}개({', '.join(targets) or '없음'}) · workspace 크레이트 "
               f"{len(members)}개 · 위반 {len(violations)}개")
    return violations, len(targets), summary


def self_test():
    names = {}

    def pid(name, source="path+file:///ws/crates/"):
        i = f"{source}{name}#0.1.0" if source.startswith("path") else f"{source}#{name}@9.0.0"
        names[i] = name
        return i

    def meta(members, nodes):
        """members: workspace 크레이트 이름. nodes: {id: [(의존 id, 이름, 종류 또는 None)]}."""
        ids = set(nodes)
        for deps in nodes.values():
            ids |= {d for d, _, _ in deps}
        ids |= {pid(m) for m in members}
        return {
            "workspace_members": [pid(m) for m in members],
            "packages": [{"id": i, "name": names[i]} for i in sorted(ids)],
            "resolve": {"nodes": [
                {"id": i, "deps": [{"name": n, "pkg": d, "dep_kinds": [{"kind": k, "target": None}]}
                                   for d, n, k in nodes.get(i, [])]}
                for i in sorted(ids)
            ]},
        }

    rv, p2p, ident, frame, ssh, transport = (pid(n) for n in (
        "tasty-net-rendezvous", "tasty-net-p2p", "tasty-net-identity", "tasty-net-frame",
        "tasty-net-ssh", "tasty-net-transport"))
    serde = pid("serde", "registry+https://github.com/rust-lang/crates.io-index")
    other_frame = pid("tasty-net-frame", "registry+https://github.com/rust-lang/crates.io-index")
    helper = pid("helper", "registry+https://github.com/rust-lang/crates.io-index")
    ws = ["tasty-net-rendezvous", "tasty-net-identity", "tasty-net-frame", "tasty-net-ssh", "tasty-net-transport"]
    cases = [
        ("rendezvous → identity만", meta(ws, {rv: [(ident, "tasty-net-identity", None), (serde, "serde", None)],
                                           ident: [(serde, "serde", None)]}), 0),
        ("p2p → rendezvous·identity", meta(ws + ["tasty-net-p2p"], {p2p: [(rv, "tasty-net-rendezvous", None),
                                                                        (ident, "tasty-net-identity", "dev")],
                                                                  rv: [(ident, "tasty-net-identity", None)]}), 0),
        ("rendezvous → ssh 일반 의존", meta(ws, {rv: [(ssh, "tasty-net-ssh", None)]}), 1),
        ("rendezvous → frame dev 의존", meta(ws, {rv: [(frame, "tasty-net-frame", "dev")]}), 1),
        ("rendezvous → transport build 의존", meta(ws, {rv: [(transport, "tasty-net-transport", "build")],
                                                        transport: [(frame, "tasty-net-frame", None)]}), 1),
        ("rendezvous → p2p(방향이 거꾸로)", meta(ws + ["tasty-net-p2p"], {rv: [(p2p, "tasty-net-p2p", "dev")],
                                                                        p2p: [(rv, "tasty-net-rendezvous", None)]}), 1),
        ("identity를 거쳐 frame", meta(ws, {rv: [(ident, "tasty-net-identity", None)],
                                          ident: [(frame, "tasty-net-frame", None)]}), 1),
        ("identity의 dev 의존은 전파하지 않는다", meta(ws, {rv: [(ident, "tasty-net-identity", None)],
                                                       ident: [(frame, "tasty-net-frame", "dev")]}), 0),
        ("이름이 같은 다른 출처 크레이트는 workspace 의존이 아니다",
         meta(ws, {rv: [(other_frame, "tasty-net-frame", None)]}), 0),
        ("이름을 바꿔 쓴 workspace 의존", meta(ws, {rv: [(frame, "frame", None)]}), 1),
        ("workspace 밖 크레이트를 거쳐 닿는 workspace 크레이트",
         meta(ws, {rv: [(helper, "helper", None)], helper: [(ssh, "tasty-net-ssh", None)]}), 1),
    ]
    no_resolve = meta(ws, {rv: [(frame, "tasty-net-frame", "dev")]})
    no_resolve["resolve"] = None
    no_node = meta(ws, {rv: [(frame, "tasty-net-frame", "dev")]})
    no_node["resolve"]["nodes"] = [n for n in no_node["resolve"]["nodes"] if n["id"] != rv]
    cases += [
        ("resolve가 null이면(--no-deps 출력) 판정 불가", no_resolve, "판정 불가"),
        ("대상 크레이트의 노드가 그래프에 없으면 판정 불가", no_node, "판정 불가"),
    ]
    failed = 0
    for title, m, want in cases:
        try:
            violations, _, _ = judge(m)
            got = len(violations)
        except Undecidable as e:
            violations, got = [str(e)], "판정 불가"
        if got != want:
            print(f"  ✘ {title}: 결과 {got} {violations}, 기대 {want}", file=sys.stderr)
            failed += 1
    _, targets, _ = judge(meta(["tasty-net-identity", "tasty-net-frame"], {ident: []}))
    if targets != 0:
        print("  ✘ 대상 크레이트가 없으면 대상 0개여야 한다", file=sys.stderr)
        failed += 1
    total = len(cases) + 1
    if failed:
        print(f"[p2p-isolation] 자체 시험 실패 — 사례 {total}개 중 {failed}개", file=sys.stderr)
        sys.exit(1)
    print(f"[p2p-isolation] 자체 시험 통과 — 사례 {total}개")


mode, tmp = sys.argv[1], sys.argv[2]
if mode == "self-test":
    self_test()
    sys.exit(0)

with open(os.path.join(tmp, "metadata.json"), encoding="utf-8") as f:
    meta = json.load(f)
try:
    violations, targets, summary = judge(meta)
except Undecidable as e:
    print(f"[p2p-isolation] 판정 불가: {e}", file=sys.stderr)
    sys.exit(2)
if targets == 0:
    print(f"[p2p-isolation] 판정 불가: 대상 크레이트가 0개다({', '.join(sorted(ALLOWED))}).", file=sys.stderr)
    sys.exit(2)
if violations:
    for v in violations:
        print(f"  {v}", file=sys.stderr)
    print(f"[p2p-isolation] 위반 — {summary}", file=sys.stderr)
    print("  P2P 크레이트는 workspace 안에서 identity와 (p2p만) rendezvous에만 의존한다(docs/design/crates.md \"P2P 크레이트 격리\").", file=sys.stderr)
    sys.exit(1)
print(f"[p2p-isolation] 통과 — {summary}")
PY
