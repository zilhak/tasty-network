#!/usr/bin/env bash
# 이 workspace가 tasty 레포의 크레이트에 의존하지 않는지 cargo metadata로 본다(설계 문서 D7).
# 위반: source가 tasty 레포(github.com/zilhak/tasty)를 가리키는 git 의존,
#       source가 없고(path 의존) manifest_path가 이 workspace 루트 밖인 패키지.
# 이 workspace 안의 path 의존과 workspace 멤버는 위반이 아니다.
#
# 사용: bash scripts/check-no-tasty-deps.sh
#       bash scripts/check-no-tasty-deps.sh --metadata <파일>   판정만 시험할 때 cargo metadata 출력 JSON을 준다
# 종료 코드: 통과 0, 위반 1, 판정 불가(도구 없음·metadata 실패·패키지 0개) 2.

set -euo pipefail

cd "$(dirname "$0")/.."

PY=""
for c in python3 python; do
    if command -v "$c" >/dev/null 2>&1; then PY="$c"; break; fi
done
[ -n "$PY" ] || { echo "[no-tasty-deps] 판정 불가: python3가 PATH에 없다." >&2; exit 2; }

TMP="$(mktemp -d)"
trap 'rm -rf "${TMP:?}"' EXIT

if [ "${1:-}" = --metadata ]; then
    [ -f "${2:-}" ] || { echo "[no-tasty-deps] 판정 불가: metadata 파일이 없다: ${2:-}" >&2; exit 2; }
    cp "$2" "$TMP/metadata.json"
elif [ "$#" -gt 0 ]; then
    echo "[no-tasty-deps] 모르는 인자: $*" >&2
    exit 2
else
    # 선택 의존과 모든 target의 의존이 들어가도록 --all-features, --filter-platform 없이 읽는다.
    cargo metadata --format-version 1 --locked --all-features > "$TMP/metadata.json" || {
        echo "[no-tasty-deps] 판정 불가: cargo metadata가 실패했다." >&2
        exit 2
    }
fi

"$PY" - "$TMP/metadata.json" <<'PY'
import json
import os
import re
import sys

with open(sys.argv[1], encoding="utf-8") as f:
    meta = json.load(f)

def norm(p):
    return os.path.normcase(os.path.realpath(p))

root = norm(meta["workspace_root"])
packages = meta.get("packages", [])
if not packages:
    print("[no-tasty-deps] 판정 불가: cargo metadata에 패키지가 0개다.", file=sys.stderr)
    sys.exit(2)

# git+https://github.com/zilhak/tasty.git?rev=..#sha, git+ssh://git@github.com/zilhak/tasty 등.
# tasty-network처럼 이름이 tasty로 시작하는 다른 레포는 위반이 아니다.
TASTY_GIT = re.compile(r"^git\+[a-z+]+://(?:[^@/]+@)?github\.com[:/]zilhak/tasty(?:\.git)?/?$")

violations = []
git_deps = path_inside = 0
for pkg in packages:
    src = pkg.get("source")
    if src is None:
        manifest_dir = norm(os.path.dirname(pkg["manifest_path"]))
        try:
            inside = os.path.commonpath([root, manifest_dir]) == root
        except ValueError:
            # Windows에서 드라이브가 다르면 공통 경로가 없다. 그러면 workspace 밖이다.
            inside = False
        if inside:
            path_inside += 1
        else:
            violations.append(f"path 의존이 workspace 밖이다: {pkg['name']} {pkg['version']} ({pkg['manifest_path']})")
    elif src.startswith("git+"):
        git_deps += 1
        url = re.split(r"[?#]", src, maxsplit=1)[0].lower()
        if TASTY_GIT.match(url):
            violations.append(f"tasty 레포의 git 의존이다: {pkg['name']} {pkg['version']} ({src})")

summary = (f"패키지 {len(packages)}개 · workspace 안 path {path_inside}개 · git {git_deps}개 · "
           f"위반 {len(violations)}개 (루트 {meta['workspace_root']})")
if violations:
    for v in violations:
        print(f"  {v}", file=sys.stderr)
    print(f"[no-tasty-deps] 위반 — {summary}", file=sys.stderr)
    print("  tasty 레포의 크레이트에는 경로·git 어느 방식으로도 의존하지 않는다(docs/design/crates.md \"의존 방향\").", file=sys.stderr)
    sys.exit(1)
print(f"[no-tasty-deps] 통과 — {summary}")
PY
