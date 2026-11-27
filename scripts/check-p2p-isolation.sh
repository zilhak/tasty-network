#!/usr/bin/env bash
# P2P 크레이트 격리 검사(docs/design/crates.md "P2P 크레이트 격리", 동작 수준 기준 D8).
# tasty-net-p2p·tasty-net-rendezvous가 이 workspace 안에서 의존하는 크레이트가 허용 범위 안인지
# cargo metadata로 본다. 일반·build·dev 의존을 모두 보고, 허용된 workspace 크레이트를 거쳐
# 닿는 workspace 크레이트(그 크레이트의 일반·build 의존)도 본다.
#   허용: tasty-net-rendezvous → tasty-net-identity
#         tasty-net-p2p        → tasty-net-identity, tasty-net-rendezvous
#
# 사용: bash scripts/check-p2p-isolation.sh
#       bash scripts/check-p2p-isolation.sh --metadata <파일>   cargo metadata 출력 JSON 파일로 판정한다
#       bash scripts/check-p2p-isolation.sh --self-test         판정 함수를 고정 사례로 시험한다
# 종료 코드: 통과 0, 위반(또는 자체 시험 실패) 1, 판정 불가(도구 없음·metadata 실패·대상 크레이트 0개) 2.

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


def kind_name(kind):
    return kind or "normal"


def judge(meta):
    """(위반 목록, 대상 수, 요약). 대상 크레이트가 0개면 대상 수 0."""
    members = set(meta.get("workspace_members", []))
    by_id = {p["id"]: p for p in meta.get("packages", [])}
    member_names = {by_id[m]["name"] for m in members if m in by_id}
    by_name = {by_id[m]["name"]: by_id[m] for m in members if m in by_id}

    def workspace_deps(pkg, kinds):
        out = []
        for d in pkg.get("dependencies", []):
            name = d["name"]
            if name in member_names and kind_name(d.get("kind")) in kinds:
                out.append((name, kind_name(d.get("kind"))))
        return out

    violations = []
    targets = sorted(n for n in ALLOWED if n in by_name)
    for t in targets:
        allowed = ALLOWED[t]
        # 대상의 직접 의존: 일반·build·dev.
        stack = []
        for name, kind in workspace_deps(by_name[t], {"normal", "build", "dev"}):
            if name not in allowed:
                violations.append(f"{t} → {name} ({kind} 의존)")
            else:
                stack.append((name, [t, name]))
        # 허용된 크레이트를 거쳐 닿는 크레이트: 그 크레이트의 일반·build 의존.
        seen = set()
        while stack:
            name, path = stack.pop()
            if name in seen:
                continue
            seen.add(name)
            for dep, kind in workspace_deps(by_name[name], {"normal", "build"}):
                if dep not in allowed and dep != t:
                    violations.append(f"{t} → {' → '.join(path[1:])} → {dep} ({kind} 의존을 거침)")
                elif dep == t:
                    violations.append(f"{t} → {' → '.join(path[1:])} → {t} (순환)")
                else:
                    stack.append((dep, path + [dep]))
    summary = f"대상 크레이트 {len(targets)}개({', '.join(targets) or '없음'}) · workspace 크레이트 {len(member_names)}개 · 위반 {len(violations)}개"
    return violations, len(targets), summary


def self_test():
    def pkg(name, deps):
        return {
            "id": f"{name} 0.1.0 (path+file:///ws/crates/{name})",
            "name": name,
            "dependencies": [{"name": n, "kind": k} for n, k in deps],
        }

    def meta(*pkgs):
        extra = [pkg("serde", [])]
        return {
            "workspace_members": [p["id"] for p in pkgs],
            "packages": list(pkgs) + extra,
        }

    ident = pkg("tasty-net-identity", [("serde", None)])
    frame = pkg("tasty-net-frame", [])
    ssh = pkg("tasty-net-ssh", [])
    transport = pkg("tasty-net-transport", [("tasty-net-frame", None)])
    cases = [
        ("rendezvous → identity만", meta(pkg("tasty-net-rendezvous", [("tasty-net-identity", None), ("serde", None)]), ident), 0),
        ("p2p → rendezvous·identity", meta(pkg("tasty-net-p2p", [("tasty-net-rendezvous", None), ("tasty-net-identity", "dev")]),
                                         pkg("tasty-net-rendezvous", [("tasty-net-identity", None)]), ident), 0),
        ("rendezvous → ssh 일반 의존", meta(pkg("tasty-net-rendezvous", [("tasty-net-ssh", None)]), ssh), 1),
        ("rendezvous → frame dev 의존", meta(pkg("tasty-net-rendezvous", [("tasty-net-frame", "dev")]), frame), 1),
        ("rendezvous → transport build 의존", meta(pkg("tasty-net-rendezvous", [("tasty-net-transport", "build")]), transport, frame), 1),
        ("rendezvous → p2p(방향이 거꾸로)", meta(pkg("tasty-net-rendezvous", [("tasty-net-p2p", "dev")]),
                                              pkg("tasty-net-p2p", [("tasty-net-rendezvous", None)])), 1),
        ("identity를 거쳐 frame", meta(pkg("tasty-net-rendezvous", [("tasty-net-identity", None)]),
                                     pkg("tasty-net-identity", [("tasty-net-frame", None)]), frame), 1),
        ("identity의 dev 의존은 전파하지 않는다", meta(pkg("tasty-net-rendezvous", [("tasty-net-identity", None)]),
                                                   pkg("tasty-net-identity", [("tasty-net-frame", "dev")]), frame), 0),
    ]
    failed = 0
    for title, m, want in cases:
        violations, _, _ = judge(m)
        if len(violations) != want:
            print(f"  ✘ {title}: 위반 {len(violations)}개 {violations}, 기대 {want}개", file=sys.stderr)
            failed += 1
    _, targets, _ = judge(meta(ident, frame))
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
    violations, targets, summary = judge(json.load(f))
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
