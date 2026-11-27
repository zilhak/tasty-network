#!/usr/bin/env bash
# 이 workspace가 tasty 레포의 크레이트에 의존하지 않는지 cargo metadata로 본다(설계 문서 D7).
# 위반: source가 tasty 레포(GitHub의 zilhak/tasty)를 가리키는 git 의존,
#       source가 없고(path 의존) manifest_path가 이 workspace 루트 밖인 패키지.
# 이 workspace 안의 path 의존과 workspace 멤버는 위반이 아니다.
#
# 사용: bash scripts/check-no-tasty-deps.sh
#       bash scripts/check-no-tasty-deps.sh --metadata <파일>   cargo metadata 출력 JSON 파일로 판정한다
#       bash scripts/check-no-tasty-deps.sh --self-test         판정 함수를 고정 사례로 시험한다
# 종료 코드: 통과 0, 위반(또는 자체 시험 실패) 1, 판정 불가(도구 없음·metadata 실패·패키지 0개) 2.

set -euo pipefail
# cd가 CDPATH로 폴더를 찾으면 다른 폴더로 갈 수 있고 그 경로를 stdout에 찍는다($(cd … && pwd)의 값은
# 두 줄이 된다). 이 스크립트의 cd는 모두 주어진 경로 그대로 간다.
unset CDPATH

cd "$(dirname "$0")/.."

PY=""
for c in python3 python; do
    if command -v "$c" >/dev/null 2>&1; then PY="$c"; break; fi
done
[ -n "$PY" ] || { echo "[no-tasty-deps] 판정 불가: python3가 PATH에 없다." >&2; exit 2; }

TMP="$(mktemp -d)"
trap 'rm -rf "${TMP:?}"' EXIT

MODE=check
case "${1:-}" in
    --self-test)
        MODE=self-test
        ;;
    --metadata)
        [ -f "${2:-}" ] || { echo "[no-tasty-deps] 판정 불가: metadata 파일이 없다: ${2:-}" >&2; exit 2; }
        cp "$2" "$TMP/metadata.json"
        ;;
    "")
        # 선택 의존과 모든 target의 의존이 들어가도록 --all-features, --filter-platform 없이 읽는다.
        cargo metadata --format-version 1 --locked --all-features > "$TMP/metadata.json" || {
            echo "[no-tasty-deps] 판정 불가: cargo metadata가 실패했다." >&2
            exit 2
        }
        ;;
    *)
        echo "[no-tasty-deps] 모르는 인자: $*" >&2
        exit 2
        ;;
esac

"$PY" - "$MODE" "$TMP" <<'PY'
import json
import os
import re
import sys
import tempfile

# GitHub가 git에 쓰는 호스트. ssh.github.com은 443번 포트로 SSH를 받는 공식 주소다.
GITHUB_HOSTS = {"github.com", "www.github.com", "ssh.github.com"}
TASTY_REPO = "zilhak/tasty"


def repo_of_git_source(src):
    """git 출처 문자열에서 (호스트, 저장소 경로)를 꺼낸다. 소문자, .git 접미사와 양끝 '/' 없이."""
    url = src[len("git+"):] if src.startswith("git+") else src
    url = re.split(r"[?#]", url, maxsplit=1)[0]
    m = re.match(r"^[A-Za-z][A-Za-z0-9+.-]*://(?:[^@/]*@)?([^/:]+)(?::\d*)?(/.*)?$", url)
    if m:
        host, path = m.group(1), m.group(2) or ""
    else:
        # scp 꼴: [user@]host:path
        m = re.match(r"^(?:[^@/]*@)?([^/:]+):(.*)$", url)
        if not m:
            return None, None
        host, path = m.group(1), m.group(2)
    path = path.strip("/").lower()
    if path.endswith(".git"):
        path = path[: -len(".git")]
    return host.lower(), path.strip("/")


def is_tasty_git(src):
    host, path = repo_of_git_source(src)
    return host in GITHUB_HOSTS and path == TASTY_REPO


def norm(p):
    return os.path.normcase(os.path.realpath(p))


def inside_root(root, manifest_path):
    manifest_dir = norm(os.path.dirname(manifest_path))
    try:
        return os.path.commonpath([root, manifest_dir]) == root
    except ValueError:
        # Windows에서 드라이브가 다르면 공통 경로가 없다. 그러면 workspace 밖이다.
        return False


def judge(meta):
    """(위반 목록, 요약 문자열). 패키지가 0개면 None."""
    root = norm(meta["workspace_root"])
    packages = meta.get("packages", [])
    if not packages:
        return None
    violations = []
    git_deps = path_inside = 0
    for pkg in packages:
        src = pkg.get("source")
        if src is None:
            if inside_root(root, pkg["manifest_path"]):
                path_inside += 1
            else:
                violations.append(f"path 의존이 workspace 밖이다: {pkg['name']} {pkg['version']} ({pkg['manifest_path']})")
        elif src.startswith("git+"):
            git_deps += 1
            if is_tasty_git(src):
                violations.append(f"tasty 레포의 git 의존이다: {pkg['name']} {pkg['version']} ({src})")
    summary = (f"패키지 {len(packages)}개 · workspace 안 path {path_inside}개 · git {git_deps}개 · "
               f"위반 {len(violations)}개 (루트 {meta['workspace_root']})")
    return violations, summary


def self_test(tmp):
    tasty = [
        "git+https://github.com/zilhak/tasty",
        "git+https://github.com/zilhak/tasty.git",
        "git+https://github.com/zilhak/tasty/",
        "git+https://github.com/zilhak/tasty.git?rev=0123abc#0123abcdef",
        "git+https://GitHub.com/Zilhak/Tasty.GIT",
        "git+https://user:token@github.com/zilhak/tasty",
        "git+https://www.github.com/zilhak/tasty",
        "git+ssh://git@github.com/zilhak/tasty.git",
        "git+ssh://git@github.com:22/zilhak/tasty",
        "git+ssh://git@ssh.github.com:443/zilhak/tasty.git",
        "git+ssh://git@SSH.GitHub.com:443/Zilhak/Tasty",
        "git@github.com:zilhak/tasty",
        "git@github.com:zilhak/tasty.git",
        "git+git@github.com:zilhak/Tasty.git",
    ]
    not_tasty = [
        "git+https://github.com/zilhak/tasty-network",
        "git+https://github.com/zilhak/tasty-network.git#abc",
        "git+ssh://git@ssh.github.com:443/zilhak/tasty-network.git",
        "git@github.com:zilhak/tasty-official-plugins.git",
        "git+https://github.com/zilhak/tastyx",
        "git+https://github.com/other/tasty",
        "git+https://github.com/zilhak/tasty/extra",
        "git+https://example.com/zilhak/tasty",
        "git+https://github.com.example.com/zilhak/tasty",
    ]
    failed = 0
    for src in tasty:
        if not is_tasty_git(src):
            print(f"  ✘ tasty로 판정해야 한다: {src}", file=sys.stderr)
            failed += 1
    for src in not_tasty:
        if is_tasty_git(src):
            print(f"  ✘ tasty가 아니라고 판정해야 한다: {src}", file=sys.stderr)
            failed += 1

    root = os.path.join(tmp, "ws")
    for d in ("ws/crates/a", "ws-evil/b", "outside/c"):
        os.makedirs(os.path.join(tmp, d))
    try:
        os.symlink(os.path.join(tmp, "outside"), os.path.join(root, "link"))
        has_link = True
    except (OSError, NotImplementedError):
        # Windows에서 권한이 없으면 링크를 만들 수 없다. 그 사례만 뺀다.
        has_link = False

    def pkg(name, manifest, source=None):
        return {"name": name, "version": "0.1.0", "source": source, "manifest_path": manifest}

    member = pkg("member", os.path.join(root, "crates/a/Cargo.toml"))
    reg = pkg("serde", "/registry/serde/Cargo.toml", "registry+https://github.com/rust-lang/crates.io-index")
    cases = [
        ("멤버와 레지스트리만", [member, reg], 0),
        ("tasty git", [member, pkg("tasty-ipc", "/git/x/Cargo.toml", tasty[8])], 1),
        ("tasty-network git", [member, pkg("tasty-net-x", "/git/y/Cargo.toml", not_tasty[0])], 0),
        ("workspace 밖 path", [member, pkg("c", os.path.join(tmp, "outside/c/Cargo.toml"))], 1),
        ("이름 앞부분만 같은 형제 폴더", [member, pkg("b", os.path.join(tmp, "ws-evil/b/Cargo.toml"))], 1),
    ]
    if has_link:
        cases.append(("workspace 안 링크로 밖을 가리키는 path", [member, pkg("c", os.path.join(root, "link/c/Cargo.toml"))], 1))
    for title, packages, want in cases:
        violations, _ = judge({"workspace_root": root, "packages": packages})
        if len(violations) != want:
            print(f"  ✘ {title}: 위반 {len(violations)}개, 기대 {want}개", file=sys.stderr)
            failed += 1
    if judge({"workspace_root": root, "packages": []}) is not None:
        print("  ✘ 패키지 0개는 판정 불가여야 한다", file=sys.stderr)
        failed += 1

    total = len(tasty) + len(not_tasty) + len(cases) + 1
    if failed:
        print(f"[no-tasty-deps] 자체 시험 실패 — 사례 {total}개 중 {failed}개", file=sys.stderr)
        sys.exit(1)
    print(f"[no-tasty-deps] 자체 시험 통과 — 사례 {total}개 (tasty URL {len(tasty)} · 다른 URL {len(not_tasty)} · metadata {len(cases) + 1})")


mode, tmp = sys.argv[1], sys.argv[2]
if mode == "self-test":
    self_test(tmp)
    sys.exit(0)

with open(os.path.join(tmp, "metadata.json"), encoding="utf-8") as f:
    result = judge(json.load(f))
if result is None:
    print("[no-tasty-deps] 판정 불가: cargo metadata에 패키지가 0개다.", file=sys.stderr)
    sys.exit(2)
violations, summary = result
if violations:
    for v in violations:
        print(f"  {v}", file=sys.stderr)
    print(f"[no-tasty-deps] 위반 — {summary}", file=sys.stderr)
    print("  tasty 레포의 크레이트에는 경로·git 어느 방식으로도 의존하지 않는다(docs/design/crates.md \"의존 방향\").", file=sys.stderr)
    sys.exit(1)
print(f"[no-tasty-deps] 통과 — {summary}")
PY
