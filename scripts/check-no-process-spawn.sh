#!/usr/bin/env bash
# 다른 프로그램 실행 금지의 정적 검사(docs/design/mobile.md "정적 검사 no-process-spawn").
# 모바일 target에서 컴파일되는 라이브러리 코드가 std::process의 실행 항목(Command·Child·Stdio 등),
# std::os::<os>::process::CommandExt, tokio::process를 쓰지 않는지 본다.
#
# 사용: bash scripts/check-no-process-spawn.sh               이 레포의 금지 크레이트를 검사한다
#       bash scripts/check-no-process-spawn.sh --self-test   판정 함수를 고정 사례로 시험한다
# 종료 코드: 통과 0, 위반(또는 자체 시험 실패) 1, 판정 불가(도구 없음·대상 0개) 2.
#
# 보는 파일과 지우는 자리, 실패로 보는 모양의 정본은 위 설계 절이다. 요약:
#   - 금지 크레이트(아래 CRATES)의 src/**/*.rs에서 src/bin/**와 src/main.rs를 뺀다. 없는 크레이트는
#     건너뛴다(앞으로 생길 ssh-config·ffi).
#   - tasty-net-ssh의 src/system/**는 보지 않는다. 대신 src/lib.rs의 mod system; 선언에
#     #[cfg(feature = "system-ssh")]가 없으면 실패다.
#   - #[cfg(test)]·#[cfg(all(test, …))] 항목과 그런 속성으로 선언한 파일 모듈은 보지 않는다
#     (scripts/lib/rust_scan.py). 문자열·주석 안의 글자도 보지 않는다.
#   - use 선언은 나무로 펼쳐 가져온 경로마다 판정하고, use 밖에 적은 경로도 찾는다.
set -euo pipefail
# cd가 CDPATH로 폴더를 찾으면 다른 폴더로 갈 수 있고 그 경로를 stdout에 찍는다($(cd … && pwd)의 값은
# 두 줄이 된다). 이 스크립트의 cd는 모두 주어진 경로 그대로 간다.
unset CDPATH

cd "$(dirname "$0")/.."

PY=""
for c in python3 python; do
    if command -v "$c" >/dev/null 2>&1; then PY="$c"; break; fi
done
[ -n "$PY" ] || { echo "[no-process-spawn] 판정 불가: python3가 PATH에 없다." >&2; exit 2; }

MODE=check
case "${1:-}" in
    --self-test) MODE=self-test ;;
    "") ;;
    *)
        echo "[no-process-spawn] 모르는 인자: $*" >&2
        exit 2
        ;;
esac

"$PY" - "$MODE" <<'PY'
import os
import sys

sys.dont_write_bytecode = True
sys.path.insert(0, os.path.join(os.getcwd(), "scripts", "lib"))
from rust_scan import (  # noqa: E402  (scripts/lib/rust_scan.py)
    KEYWORDS, ScanError, matching_close, strip_cfg_test, test_module_files, tokenize,
)

TAG = "[no-process-spawn]"

# 금지 크레이트(설계 "다른 프로그램 실행 금지" 경계 표). rendezvous는 라이브러리 target만 본다.
CRATES = [
    "tasty-net-frame", "tasty-net-identity", "tasty-net-topology", "tasty-net-ssh",
    "tasty-net-ssh-config", "tasty-net-transport", "tasty-net-p2p", "tasty-net-rendezvous",
    "tasty-net-ffi",
]
SSH_CRATE = "tasty-net-ssh"
SSH_SYSTEM_FEATURE = "system-ssh"

# std::process 아래에서 가져와도 되는 이름. 그 밖은 모두 실패다.
STD_PROCESS_OK = {"id", "exit", "abort", "ExitCode"}
# use 밖에 적은 경로에서 찾는 금지 이름.
STD_PROCESS_BAD = {"Command", "Child", "Stdio", "ChildStdin", "ChildStdout", "ChildStderr",
                   "ExitStatus"}


# 이 크레이트 안을 가리키는 경로의 첫 조각. crate::std::…·self::std::…·super::std::…·$crate::std::…의
# std는 이 크레이트 안의 항목이다. 그 항목이 표준 라이브러리를 가리키게 하는 방법(use std;·use std as x;·
# extern crate std;)은 그 선언에서 실패하고, 이 크레이트의 모듈이면 그 파일을 따로 본다.
LOCAL_ROOTS = {"crate", "self", "super", "Self"}


def forbidden_paths(path):
    """path(조각 목록)에 견줄 금지 경로들. std::os::<os>::process는 그 path의 <os> 자리로 채운다."""
    out = [["std", "process"], ["tokio", "process"]]
    os_name = path[2] if len(path) >= 3 and path[:2] == ["std", "os"] else "<os>"
    out.append(["std", "os", os_name, "process"])
    return out


def judge_import(path, glob):
    """펼친 use 항목 하나(가져온 경로, 글롭인지)의 판정. 실패면 이유 문자열, 통과면 None."""
    if not path:
        return None
    if glob and len(path) == 4 and path[:2] == ["std", "os"] and path[3] == "prelude":
        return "std::os::<os>::prelude의 글롭"
    for bad in forbidden_paths(path):
        if len(path) <= len(bad) and bad[:len(path)] == path:
            what = "글롭" if glob else "모듈·윗 경로를 지역 이름으로 들임"
            return f"금지 경로 {'::'.join(bad)}의 {what}"
        if len(path) > len(bad) and path[:len(bad)] == bad:
            name = path[len(bad)]
            if bad == ["std", "process"]:
                if glob or len(path) > len(bad) + 1 or name not in STD_PROCESS_OK:
                    return "std::process 아래에서 id·exit·abort·ExitCode 밖의 항목"
            elif bad[0] == "tokio":
                return "tokio::process 아래 항목"
            elif name == "CommandExt":
                return "CommandExt"
    return None


def parse_use_tree(toks, i, prefix, out):
    """toks[i]에서 시작하는 use 나무를 펼쳐 out에 (경로, 글롭인지, 줄)을 더한다. 다음 위치를 돌려준다."""
    path = list(prefix)
    if i < len(toks) and toks[i][1] == "::":
        i += 1
    if (not path and i + 2 < len(toks) and toks[i][1] == "$" and toks[i + 1][1] == "crate"
            and toks[i + 2][1] == "::"):
        # 매크로 본문의 use $crate::…는 crate::…처럼 매크로가 정의된 크레이트 안의 지역 경로다.
        path = ["$crate"]
        i += 3
    while i < len(toks):
        k, t, line = toks[i]
        if t == "*":
            out.append((path, True, line))
            return i + 1
        if t == "{":
            close = matching_close(toks, i)
            j = i + 1
            while j < close:
                j = parse_use_tree(toks, j, path, out)
                if j < close and toks[j][1] == ",":
                    j += 1
                elif j < close:
                    raise ScanError(f"{line}행: use 묶음을 읽지 못했다(토큰 {toks[j][1]!r})")
            return close + 1
        if k != "ident":
            raise ScanError(f"{line}행: use 경로를 읽지 못했다(토큰 {t!r})")
        if t == "self" and path:
            seg_path = list(path)
        else:
            seg_path = path + [t]
        i += 1
        if i < len(toks) and toks[i][1] == "::":
            path = seg_path
            i += 1
            continue
        if i < len(toks) and toks[i][1] == "as":
            i += 2
        out.append((seg_path, False, line))
        return i
    raise ScanError("use 선언이 끝나지 않았다")


def use_items(toks):
    """(시작, 끝) 범위와 함께 펼친 use 항목 목록. 범위는 use 밖 경로 찾기에서 뺀다."""
    items, spans = [], []
    i = 0
    while i < len(toks):
        if toks[i][:2] == ("ident", "use") and not (i + 1 < len(toks) and toks[i + 1][1] == "<"):
            # use<…>는 정밀 캡처(impl Trait + use<'a>)이지 import가 아니다.
            end = i
            depth = 0
            while end < len(toks):
                t = toks[end][1]
                if t == "{":
                    depth += 1
                elif t == "}":
                    depth -= 1
                elif t == ";" and depth == 0:
                    break
                end += 1
            got = []
            parse_use_tree(toks[:end], i + 1, [], got)
            items += got
            spans.append((i, end))
            i = end + 1
            continue
        i += 1
    return items, spans


def path_hits(toks, spans):
    """use 밖에 적은 금지 경로와 크레이트 별칭 선언: (줄, 경로 글자, 이유)."""
    skip = set()
    for a, b in spans:
        skip.update(range(a, b + 1))
    hits = []
    n = len(toks)

    def words(i, count):
        out, j = [], i
        while len(out) < count and j < n:
            if toks[j][0] != "ident":
                return None
            out.append(toks[j][1])
            if len(out) < count:
                if j + 1 >= n or toks[j + 1][1] != "::":
                    return None
                j += 2
            else:
                j += 1
        return out if len(out) == count else None

    for i in range(n):
        if i in skip or toks[i][0] != "ident":
            continue
        if (i > 1 and toks[i - 1][1] == "::"
                and ((toks[i - 2][0] == "ident"
                      and (toks[i - 2][1] not in KEYWORDS or toks[i - 2][1] in LOCAL_ROOTS))
                     or toks[i - 2][1] == ">")):
            # 경로 한가운데(a::std::…, <T>::std::…, crate::std::…)는 그 경로의 첫 조각에서 본다. 앞에
            # ::만 있거나 그 앞이 키워드면(::std::…, return ::std::…, &mut ::std::…) 루트 경로라 여기서 본다.
            continue
        t, line = toks[i][1], toks[i][2]
        if t == "extern" and i + 2 < n and toks[i + 1][1] == "crate" \
                and toks[i + 2][1] in ("std", "tokio"):
            alias = toks[i + 4][1] if i + 4 < n and toks[i + 3][1] == "as" else None
            hits.append((line, f"extern crate {toks[i + 2][1]}" + (f" as {alias}" if alias else ""),
                         "크레이트 별칭 선언" if alias else "크레이트를 모듈 안 이름으로 들임"))
            continue
        w = words(i, 3)
        if w and w[:2] == ["std", "process"] and w[2] in STD_PROCESS_BAD:
            hits.append((line, "::".join(w), "std::process의 실행 항목"))
            continue
        if w and w[:2] == ["tokio", "process"]:
            hits.append((line, "::".join(w), "tokio::process 아래 항목"))
            continue
        w = words(i, 5)
        if w and w[:2] == ["std", "os"] and w[3] == "process" and w[4] == "CommandExt":
            hits.append((line, "::".join(w), "CommandExt"))
    return hits


def judge_source(text):
    """제품 코드 글자 하나의 위반: (줄, 경로 글자, 이유) 목록과 펼친 use 항목 수."""
    toks = strip_cfg_test(tokenize(text))
    items, spans = use_items(toks)
    out = []
    for path, glob, line in items:
        # include!를 use로 들이면(별칭이 있든 없든) 다른 이름으로 부를 수 있어 include_lines가 찾지 못한다.
        if not glob and len(path) >= 2 and path[0] in ("std", "core") and path[-1] == "include":
            raise ScanError(f"{line}행: {'::'.join(path)}를 use로 들인 include!는 판정하지 않는다")
        why = judge_import(path, glob)
        if why:
            out.append((line, "::".join(path) + ("::*" if glob else ""), why))
    out += path_hits(toks, spans)
    return sorted(out), len(items)


def attrs_before(toks, i):
    """toks[i](mod 등) 앞에 붙은 속성들의 글자 목록. pub·pub(…)는 건너뛴다."""
    j = i - 1
    if j >= 0 and toks[j][1] == ")":
        depth = 0
        while j >= 0:
            if toks[j][1] == ")":
                depth += 1
            elif toks[j][1] == "(":
                depth -= 1
                if depth == 0:
                    break
            j -= 1
        j -= 1
    if j >= 0 and toks[j][1] == "pub":
        j -= 1
    out = []
    while j >= 1 and toks[j][1] == "]":
        depth, k = 0, j
        while k >= 0:
            if toks[k][1] == "]":
                depth += 1
            elif toks[k][1] == "[":
                depth -= 1
                if depth == 0:
                    break
            k -= 1
        if k < 1 or toks[k - 1][1] != "#":
            break
        out.append([t[1] for t in toks[k + 1:j]])
        j = k - 2
    return out


def system_mod_guarded(text):
    """src/lib.rs의 제품 코드(시험 항목을 지운 뒤)에 mod system 선언이 꼭 하나이고 그것이 파일 모듈
    선언(mod system;)이면, 그 선언에 #[cfg(feature = "system-ssh")]가 붙어 있는지를 돌려준다. 선언이
    둘 이상이거나 인라인(mod system { … })이면 src/system/ 제외가 그 feature로만 켜지는지 말할 수
    없으므로 ScanError."""
    toks = strip_cfg_test(tokenize(text))
    decls = [i for i in range(len(toks) - 2)
             if toks[i][:2] == ("ident", "mod") and toks[i + 1][1] == "system"]
    if not decls:
        return False
    if len(decls) > 1:
        raise ScanError(f"{toks[decls[1]][2]}행: mod system 선언이 {len(decls)}개다")
    i = decls[0]
    if toks[i + 2][1] != ";":
        raise ScanError(f"{toks[i][2]}행: mod system이 인라인 모듈이다(안의 선언이 src/system/을 가리킨다)")
    want = ["cfg", "(", "feature", "=", f'"{SSH_SYSTEM_FEATURE}"', ")"]
    return want in attrs_before(toks, i)


def path_attr_lines(toks):
    """제품 코드에서 모듈 파일을 고르는 속성의 줄 목록: #[path = …]와, 안 어디에든 path = 가 있는
    #[cfg_attr(…)](겹친 cfg_attr 포함). cfg_attr의 조건이 켜지는지는 따지지 않는다."""
    out = []
    for i in range(len(toks) - 2):
        if toks[i][1] != "#" or toks[i + 1][1] != "[":
            continue
        head = toks[i + 2][:2]
        if head == ("ident", "path"):
            out.append(toks[i][2])
        elif head == ("ident", "cfg_attr"):
            close = matching_close(toks, i + 1)
            if any(toks[k][:2] == ("ident", "path") and toks[k + 1][1] == "="
                   for k in range(i + 3, close)):
                out.append(toks[i][2])
    return out


def include_lines(toks):
    """제품 코드의 include!(…) 줄 목록(std::include!처럼 경로로 부른 것 포함). include_str!·
    include_bytes!는 코드를 들이지 않으므로 아니다."""
    return [toks[i][2] for i in range(len(toks) - 1)
            if toks[i][:2] == ("ident", "include") and toks[i + 1][1] == "!"]


def crate_files(crate, listing):
    """금지 크레이트에서 볼 파일 목록. listing(crate) -> src 아래 .rs 상대 경로 목록(crates/<c>/src/…)."""
    src = f"crates/{crate}/src/"
    out = []
    for f in listing(crate):
        rel = f[len(src):]
        if rel == "main.rs" or rel.startswith("bin/"):
            continue
        if crate == SSH_CRATE and rel.startswith("system/"):
            continue
        out.append(f)
    return out


def collect(read, listing, exists):
    """판정: (위반 문자열 목록, 요약, 대상 파일 수)."""
    violations = []
    seen_crates, files_checked, use_count, excluded_hits = 0, 0, 0, 0
    for crate in CRATES:
        if not exists(crate):
            continue
        seen_crates += 1
        all_files = listing(crate)
        files = crate_files(crate, listing)
        if crate == SSH_CRATE:
            lib = f"crates/{crate}/src/lib.rs"
            try:
                guarded = system_mod_guarded(read(lib))
            except ScanError as e:
                raise ScanError(f"{lib}: {e}") from None
            if not guarded:
                violations.append(f"{lib}: mod system;에 #[cfg(feature = \"{SSH_SYSTEM_FEATURE}\")]가"
                                  " 없다(src/system/ 예외는 그 feature로만 켜져야 한다)")
        tests = test_module_files(read, files)
        checked = [f for f in files if f not in tests]
        for f in checked:
            # #[path]는 모듈 파일을 폴더 규칙 밖에서 고른다. 그러면 src/system/ 제외와 시험 모듈 파일
            # 판정이 실제로 컴파일되는 파일과 맞는지 말할 수 없다.
            try:
                product = strip_cfg_test(tokenize(read(f)))
                hits, uses = judge_source(read(f))
            except ScanError as e:
                raise ScanError(f"{f}: {e}") from None
            lines = path_attr_lines(product)
            if lines:
                raise ScanError(f"{f}:{lines[0]}: #[path]·#[cfg_attr(…, path = …)] 모듈 선언은 판정하지 않는다")
            # include!는 다른 파일의 코드를 그 자리에 들인다. 그 파일이 이 검사가 보는 파일인지
            # (src/system/ 제외, 시험 모듈 파일) 말할 수 없다.
            lines = include_lines(product)
            if lines:
                raise ScanError(f"{f}:{lines[0]}: include!는 판정하지 않는다")
            use_count += uses
            violations += [f"{f}:{line}: {p} ({why})" for line, p, why in hits]
        files_checked += len(checked)
        # 대조값: 뺀 자리(시험 항목·시험 모듈 파일·src/system·src/bin)에서 같은 판정이 찾는 수.
        for f in all_files:
            try:
                toks = tokenize(read(f))
                items, spans = use_items(toks)
            except ScanError:
                continue
            raw = [1 for p, g, _ in items if judge_import(p, g)] + path_hits(toks, spans)
            kept = 0
            if f in checked:
                kept = len(judge_source(read(f))[0])
            excluded_hits += len(raw) - kept
    summary = (f"크레이트 {seen_crates}개 · 파일 {files_checked}개 · 펼친 use 항목 {use_count}개"
               f" · 위반 {len(violations)}개 (대조: 뺀 자리의 같은 모양 {excluded_hits}건)")
    return violations, summary, files_checked


def self_test():
    bad = [
        ("std::process::Command::new", "fn f() { let _ = std::process::Command::new(\"ssh\"); }\n"),
        ("::std::process::Child", "fn f(_c: ::std::process::Child) {}\n"),
        ("use std::process::Command;", "use std::process::Command;\n"),
        ("use std::process as p;", "use std::process as p;\n"),
        ("use std::{process as proc};", "use std::{process as proc};\n"),
        ("use std::{io, process::{self, id}};", "use std::{io, process::{self, id}};\n"),
        ("use std::process::{self as p};", "use std::process::{self as p};\n"),
        ("use tokio::{process as p};", "use tokio::{process as p};\n"),
        ("use tokio::process::Command as C;", "use tokio::process::Command as C;\n"),
        ("tokio::process::Command::new", "fn f() { tokio::process::Command::new(\"x\"); }\n"),
        ("use std::process::*;", "use std::process::*;\n"),
        ("use std::process::Output;", "use std::process::Output;\n"),
        ("extern crate std as s;", "extern crate std as s;\n"),
        ("extern crate tokio as t;", "extern crate tokio as t;\n"),
        ("별칭 없는 extern crate std;(self::std가 표준 라이브러리가 된다)",
         "extern crate std;\nfn f() { self::std::process::Command::new(\"x\"); }\n"),
        ("별칭 없는 pub extern crate tokio;", "pub extern crate tokio;\n"),
        ("use std as s;", "use std as s;\n"),
        ("use ::std as s;", "use ::std as s;\n"),
        ("use std::{self as s};", "use std::{self as s};\n"),
        ("use tokio as t;", "use tokio as t;\n"),
        ("use std::os::unix as u;", "use std::os::unix as u;\n"),
        ("use std::os::unix;", "use std::os::unix;\n"),
        ("use std::os::{self as o};", "use std::os::{self as o};\n"),
        ("use std::os::unix::prelude::*;", "use std::os::unix::prelude::*;\n"),
        ("use std::os::unix::process::CommandExt as _;",
         "use std::os::unix::process::CommandExt as _;\n"),
        ("std::os::windows::process::CommandExt 경로", "fn f() { use_it::<std::os::windows::process::CommandExt>(); }\n"),
        ("use std::*;", "use std::*;\n"),
        ("use ::std::*;", "use ::std::*;\n"),
        ("use std::os::*;", "use std::os::*;\n"),
        ("use std::os::unix::*;", "use std::os::unix::*;\n"),
        ("use std::{os::*};", "use std::{os::*};\n"),
        ("use tokio::*;", "use tokio::*;\n"),
        ("여러 줄로 나눈 묶음 선언", "use std::{\n    io,\n    process::{\n        id,\n        Command,\n    },\n};\n"),
        ("cfg(any(test, unix)) 안의 금지 경로", "#[cfg(any(test, unix))]\nfn f() { std::process::Command::new(\"x\"); }\n"),
        ("raw 식별자 use r#std::r#process::r#Command as Cmd;", "use r#std::r#process::r#Command as Cmd;\n"),
        ("raw 식별자 경로 r#std::r#process::r#Child", "fn f(_c: r#std::r#process::r#Child) {}\n"),
        ("return ::std::process::Command", "fn f() -> ::std::process::Output { return ::std::process::Command::new(\"x\").output().unwrap(); }\n"),
        ("&mut ::std::process::Command", "fn f() { g(&mut ::std::process::Command::new(\"x\")); }\n"),
        ("for a in ::std::process::Command", "fn f() { for a in ::std::process::Command::new(\"x\").get_args() { h(a); } }\n"),
        ("match guard if ::std::process::Command", "fn f(x: u8) { match x { _ if ::std::process::Command::new(\"x\").status().is_ok() => {} _ => {} } }\n"),
        ("while ::tokio::process::Command", "fn f() { while ::tokio::process::Command::new(\"y\").spawn().is_err() {} }\n"),
        ("else 블록 안의 ::std::process::Command", "fn f(b: bool) -> ::std::process::Output { if b { todo!() } else { ::std::process::Command::new(\"x\").output().unwrap() } }\n"),
        ("시험 cfg 필드 뒤의 금지 경로", "struct S { #[cfg(test)] a: u8, b: u16 }\nfn g() { std::process::Command::new(\"x\"); }\n"),
        ("시험 cfg enum 변형 뒤의 금지 경로", "enum E { #[cfg(test)] A, B }\nfn g() { std::process::Command::new(\"x\"); }\n"),
        ("시험 cfg 배열 원소 뒤의 금지 경로", "fn h() -> Vec<u8> { vec![#[cfg(test)] 1, 2] }\nfn g() { std::process::Command::new(\"x\"); }\n"),
        ("cfg(all(test, unix)) 마지막 필드 뒤의 금지 경로", "struct S { b: u8, #[cfg(all(test, unix))] a: u8 }\nfn g() { std::process::Command::new(\"x\"); }\n"),
        ("시험 cfg if 문장 뒤 같은 블록의 금지 경로", "fn f(x: bool) { #[cfg(test)] if x { a(); } else { b(); } std::process::Command::new(\"x\"); }\n"),
        ("시험 cfg 매크로 문장 m! { } 뒤의 금지 경로", "fn f() { #[cfg(test)] m! { 1 } std::process::Command::new(\"x\"); }\n"),
        ("매크로 호출 인자 안의 #[cfg(test)] 모양 뒤 금지 경로",
         "macro_rules! evaluate { (#[cfg(test)] $e:expr) => { $e }; }\n"
         "fn f() { evaluate!(#[cfg(test)] std::process::Command::new(\"x\")); }\n"),
        ("Unicode 이름 매크로 호출 인자 안의 #[cfg(test)] 모양 뒤 금지 경로",
         "macro_rules! 검사 { (#[cfg(test)] $e:expr) => { $e }; }\n"
         "fn f() { 검사!(#[cfg(test)] std::process::Command::new(\"x\")); }\n"),
        ("매크로 호출 인자 [ ]·{ } 안의 #[cfg(test)] 모양 뒤 금지 경로",
         "fn f() { m![#[cfg(test)] std::process::Command::new(\"x\")]; n::k! { #[cfg(test)] std::process::Child } }\n"),
        ("시험 cfg 이름표 반복 'a: loop { } 뒤의 금지 경로",
         "fn f() { #[cfg(test)] 'a: loop { break 'a; } std::process::Command::new(\"x\").spawn(); }\n"),
        ("시험 cfg 이름표 블록 'b: { } 뒤의 금지 경로",
         "fn f() { #[cfg(test)] 'b: { } let c = std::process::Command::new(\"x\"); }\n"),
        ("시험 cfg 이름표 while 뒤의 금지 경로",
         "fn f() { #[cfg(test)] 'c: while g() {} std::process::Command::new(\"x\"); }\n"),
        ("시험 cfg 인라인 const { } 뒤의 금지 경로",
         "fn f() { #[cfg(test)] const { assert!(true) } std::process::Command::new(\"x\"); }\n"),
        ("시험 cfg 경로 매크로 문장 m::n! { } 뒤의 금지 경로",
         "fn f() { #[cfg(test)] m::n! { 1 } std::process::Command::new(\"x\"); }\n"),
    ]
    good = [
        ("use std::process::{id, exit};", "use std::process::{id, exit};\n"),
        ("use std::process::ExitCode;", "use std::process::ExitCode;\n"),
        ("use std::os::unix::fs::PermissionsExt;", "use std::os::unix::fs::PermissionsExt;\n"),
        ("use std::io;", "use std::io;\n"),
        ("pub(crate) use 와 다른 크레이트", "pub(crate) use crate::process::Command;\nuse self::a::{b as c};\n"),
        ("std::process::abort()", "fn f() -> ! { std::process::abort() }\n"),
        ("std::process::id()·exit()", "fn f() -> u32 { if false { std::process::exit(1) } std::process::id() }\n"),
        ("cfg(test) 안에만 둔 금지 경로",
         "#[cfg(test)]\nmod tests {\n    use std::process::Command;\n    fn f() { Command::new(\"x\"); }\n}\n"),
        ("cfg(all(test, unix)) 안에만 둔 금지 경로",
         "#[cfg(all(test, unix))]\nfn f() { std::process::Command::new(\"sh\"); }\n"),
        ("주석·문자열 안의 std::process::Command",
         "// std::process::Command\n/* use std::process::Command; */\nconst S: &str = \"std::process::Command\";\n"),
        ("raw 문자열(# 8개) 안의 따옴표와 std::process::Command", "const S: &str = r########\"a \"# std::process::Command \"## b\"########;\n"),
        ("raw 식별자 r#match는 키워드가 아니다", "fn r#match() {}\n"),
        ("raw C 문자열 cr##\"…\"## 안의 따옴표와 std::process::Command",
         "const S: &CStr = cr##\"a \" std::process::Command \" b\"##;\n"),
        ("raw C 문자열 cr\"…\" 안의 std::process::Command", "const S: &CStr = cr\"std::process::Command\";\n"),
        ("C 문자열 c\"…\" 안의 std::process::Command", "const S: &CStr = c\"std::process::Command\";\n"),
        ("raw 바이트 문자열 br#\"…\"# 안의 따옴표와 std::process::Command",
         "const S: &[u8] = br#\"a \" std::process::Command\"#;\n"),
        ("a::std::process::Command(다른 크레이트 안의 std 모듈)", "fn f() { other::std::process::Command::new(\"x\"); }\n"),
        ("crate::std::process::Command(이 크레이트의 std 모듈)", "fn f() { crate::std::process::Command::new(\"x\"); }\n"),
        ("self::std::process::Command(이 모듈 아래 std 모듈)", "fn f() { self::std::process::Command::new(\"x\"); }\n"),
        ("super::tokio::process::Command(윗 모듈의 tokio 모듈)", "fn f() { super::tokio::process::Command::new(\"x\"); }\n"),
        ("use $crate::std::process::Command;(매크로가 정의된 크레이트의 std 모듈)",
         "mod std { pub mod process { pub struct Command; } }\n"
         "macro_rules! m { () => { use $crate::std::process::Command; }; }\n"),
        ("use $crate::{a, std::process::Command};",
         "macro_rules! m { () => { use $crate::{a, std::process::Command}; }; }\n"),
        ("$crate::std::process::Command(매크로가 정의된 크레이트의 std 모듈)",
         "macro_rules! m { () => { $crate::std::process::Command::new(\"x\") }; }\n"),
        ("정밀 캡처 impl Copy + use<'a>는 import가 아니다",
         "fn f<'a>(x: &'a u8) -> impl Copy + use<'a> { *x }\nfn g<T>() -> impl Sized + use<T> {}\n"),
        ("시험 cfg 필드의 금지 타입만 지운다", "struct S { #[cfg(test)] c: std::process::Child, b: u8 }\n"),
        ("시험 cfg 블록 문장 안의 금지 경로", "fn f() { #[cfg(test)] { std::process::Command::new(\"x\"); } }\n"),
        ("시험 cfg 이름표 반복 안의 금지 경로", "fn f() { #[cfg(test)] 'a: loop { std::process::Command::new(\"x\"); } }\n"),
    ]
    unreadable = [
        ("시험 cfg match 갈래는 끝을 정하지 않고 판정 불가", "fn f(x: u8) { match x { #[cfg(test)] 1 => a(), _ => b() } }\n"),
        ("시험 cfg 원소의 끝을 못 찾으면(a < b, c) 판정 불가", "fn f() { let _ = [#[cfg(test)] a < b, c]; }\n"),
        ("시험 cfg 원소의 비교 a < b 뒤 원소의 > (판정 불가)",
         "fn f() { let _ = [#[cfg(test)] a < b, std::process::Command::new(\"x\") > y]; }\n"),
        ("시험 cfg 원소의 비교 a < b 뒤 다른 원소의 a > b (판정 불가)",
         "fn f() { let _ = [#[cfg(test)] a < b, { let _ = std::process::Command::new(\"x\"); true }, a > b, true]; }\n"),
        ("시험 cfg 필드의 제네릭 쉼표 HashMap<u8, u16>(비교와 가르지 못해 판정 불가)",
         "struct S { #[cfg(test)] a: HashMap<u8, u16>, b: u8 }\nfn g() { std::process::Command::new(\"x\"); }\n"),
        ("닫히지 않은 raw 문자열(판정 불가)", "const S: &str = r#\"a\";\nfn f() { std::process::Command::new(\"x\"); }\n"),
        ("닫히지 않은 문자열(판정 불가)", "fn f() { std::process::Command::new(\"x); }\n"),
        ("닫히지 않은 블록 주석(판정 불가)", "/* a\nfn f() { std::process::Command::new(\"x\"); }\n"),
        ("블록 없는 시험 cfg if 뒤의 제품 함수(판정 불가)",
         "fn g() { #[cfg(test)] if false }\nfn product() { std::process::Command::new(\"x\"); }\n"),
        ("시험 cfg 블록 안의 닫히지 않은 [(판정 불가)",
         "fn g() { #[cfg(test)] { let x = [; }\n}\nfn product() { std::process::Command::new(\"x\"); }\n"),
        ("시험 cfg else 뒤 블록 없이 닫힘(판정 불가)",
         "fn g(b: bool) { #[cfg(test)] if b { } else }\nfn product() { std::process::Command::new(\"x\"); }\n"),
        ("시험 cfg 블록이 닫히지 않는다(판정 불가)", "fn f() { #[cfg(test)] { a(); std::process::Command::new(\"x\");\n"),
        ("시험 cfg use가 ;로 끝나지 않는다(판정 불가)", "#[cfg(test)] use a::b\nfn g() { std::process::Command::new(\"x\"); }\n"),
    ]
    cases = []
    for title, src in bad:
        try:
            hits, _ = judge_source(src)
            got = "실패" if hits else "통과"
        except ScanError as e:
            got, hits = "읽기 오류", [str(e)]
        cases.append((title, got, "실패", hits))
    for title, src in good:
        try:
            hits, _ = judge_source(src)
            got = "실패" if hits else "통과"
        except ScanError as e:
            got, hits = "읽기 오류", [str(e)]
        cases.append((title, got, "통과", hits))
    for title, src in unreadable:
        try:
            hits, _ = judge_source(src)
            got = "실패" if hits else "통과"
        except ScanError as e:
            got, hits = "읽기 오류", [str(e)]
        cases.append((title, got, "읽기 오류", hits))

    # 크레이트 단위: src/system/ 예외와 그 feature 속성, 시험 모듈 파일, src/bin.
    guarded = "#[cfg(feature = \"system-ssh\")]\nmod system;\n#[cfg(test)]\nmod tests;\n"
    tree = {
        "crates/tasty-net-ssh/src/lib.rs": guarded,
        "crates/tasty-net-ssh/src/system/spawn.rs": "use std::process::{Child, Command};\n",
        "crates/tasty-net-ssh/src/tests.rs": "use std::process::Command;\n",
        "crates/tasty-net-rendezvous/src/lib.rs": "pub fn f() {}\n",
        "crates/tasty-net-rendezvous/src/bin/server.rs": "use std::process::Command;\n",
    }

    def run(files):
        def listing(c):
            return sorted(f for f in files if f.startswith(f"crates/{c}/src/"))
        return collect(lambda p: files[p], listing,
                       lambda c: any(f.startswith(f"crates/{c}/") for f in files))

    def crate_case(title, files, want):
        try:
            v, _, _ = run(files)
            got = "실패" if v else "통과"
        except ScanError as e:
            got, v = "읽기 오류", [str(e)]
        cases.append((title, got, want, v))

    crate_case("src/system/·시험 모듈 파일·src/bin 안에만 둔 금지 경로", tree, "통과")
    unguarded = dict(tree)
    unguarded["crates/tasty-net-ssh/src/lib.rs"] = "mod system;\n#[cfg(test)]\nmod tests;\n"
    crate_case("mod system;의 feature 속성을 뺀 입력", unguarded, "실패")
    other = dict(tree)
    other["crates/tasty-net-ssh/src/lib.rs"] = ("#[cfg(feature = \"embedded-ssh\")]\npub(crate) mod system;\n"
                                                "#[cfg(test)]\nmod tests;\n")
    crate_case("mod system;에 다른 feature 속성", other, "실패")
    pub_guarded = dict(tree)
    pub_guarded["crates/tasty-net-ssh/src/lib.rs"] = ("#[cfg(feature = \"system-ssh\")]\n#[doc(hidden)]\n"
                                                      "pub(crate) mod system;\n#[cfg(test)]\nmod tests;\n")
    crate_case("속성 둘과 pub(crate) mod system;", pub_guarded, "통과")
    pathed = dict(tree)
    pathed["crates/tasty-net-ssh/src/lib.rs"] = ("#[cfg(feature = \"system-ssh\")]\n#[path = \"empty.rs\"]\n"
                                                 "mod system;\n#[cfg(test)]\nmod tests;\n")
    pathed["crates/tasty-net-ssh/src/empty.rs"] = "\n"
    crate_case("feature가 붙은 mod system;을 #[path]로 다른 파일에 잇는 입력", pathed, "읽기 오류")
    second = dict(tree)
    second["crates/tasty-net-ssh/src/lib.rs"] = (guarded + "#[path = \"system/spawn.rs\"]\nmod spawn;\n")
    crate_case("feature 없는 다른 선언이 #[path]로 src/system/ 파일을 들이는 입력", second, "읽기 오류")
    inline = dict(tree)
    inline["crates/tasty-net-ssh/src/lib.rs"] = ("#[cfg(feature = \"system-ssh\")]\nmod system { pub mod spawn; }\n"
                                                 "#[cfg(test)]\nmod tests;\n")
    crate_case("인라인 mod system { … }(안의 선언이 src/system/을 가리킨다)", inline, "읽기 오류")
    twice = dict(tree)
    twice["crates/tasty-net-ssh/src/lib.rs"] = guarded + "#[cfg(not(feature = \"system-ssh\"))]\nmod system;\n"
    crate_case("mod system;이 둘(하나는 feature 없음)", twice, "읽기 오류")
    other_path = dict(tree)
    other_path["crates/tasty-net-rendezvous/src/lib.rs"] = "#[path = \"../../tasty-net-ssh/src/system/spawn.rs\"]\nmod s;\n"
    crate_case("다른 금지 크레이트의 #[path] 모듈 선언", other_path, "읽기 오류")
    cond = dict(tree)
    cond["crates/tasty-net-ssh/src/lib.rs"] = guarded + "#[cfg_attr(all(), path = \"system/spawn.rs\")]\nmod spawn;\n"
    crate_case("cfg_attr(…, path = …)로 src/system/ 파일을 들이는 입력", cond, "읽기 오류")
    cond_not = dict(tree)
    cond_not["crates/tasty-net-ssh/src/lib.rs"] = (guarded + "#[cfg_attr(not(feature = \"system-ssh\"), "
                                                   "path = \"system/spawn.rs\")]\nmod extra;\n")
    crate_case("cfg_attr(not(feature), path = …)로 src/system/ 파일을 들이는 입력", cond_not, "읽기 오류")
    nested = dict(tree)
    nested["crates/tasty-net-rendezvous/src/lib.rs"] = "#[cfg_attr(unix, cfg_attr(all(), path = \"s.rs\"))]\nmod s;\n"
    nested["crates/tasty-net-rendezvous/src/s.rs"] = "\n"
    crate_case("겹친 cfg_attr 안의 path(다른 금지 크레이트)", nested, "읽기 오류")
    included = dict(tree)
    included["crates/tasty-net-ssh/src/lib.rs"] = guarded + "include!(\"system/spawn.rs\");\n"
    crate_case("include!로 src/system/ 파일을 들이는 입력", included, "읽기 오류")
    for title, use in (("use std::include as inc;로 들인 include!", "use std::include as inc;\n"),
                       ("use std::include as import_source;로 들인 include!", "use std::include as import_source;\n"),
                       ("use core::{include as inc};로 들인 include!", "use core::{include as inc};\n"),
                       ("use ::std::include;(별칭 없음)", "use ::std::include;\n")):
        aliased = dict(tree)
        name = use.split(" as ")[1].rstrip(";}\n") if " as " in use else "include"
        aliased["crates/tasty-net-ssh/src/lib.rs"] = guarded + use + f"{name}!(\"system/spawn.rs\");\n"
        crate_case(title, aliased, "읽기 오류")
    included_path = dict(tree)
    included_path["crates/tasty-net-rendezvous/src/lib.rs"] = "std::include!(concat!(\"x\", \".rs\"));\n"
    crate_case("경로로 부른 std::include!(다른 금지 크레이트)", included_path, "읽기 오류")
    # 판정 불가 진단에 그 파일 경로가 붙는지: 닫히지 않은 raw 문자열·블록 주석을 시험 모듈 파일 찾기(모든
    # 파일)와 mod system 판정(ssh의 lib.rs)이 먼저 읽는 자리에 둔다.
    for title, path, text in (
            ("닫히지 않은 raw 문자열의 진단에 파일 경로", "crates/tasty-net-rendezvous/src/lib.rs",
             "const S: &str = r#\"a\";\n"),
            ("닫히지 않은 블록 주석의 진단에 파일 경로", "crates/tasty-net-rendezvous/src/lib.rs", "/* a\n"),
            ("ssh lib.rs의 닫히지 않은 문자열 진단에 파일 경로", "crates/tasty-net-ssh/src/lib.rs",
             guarded + "const S: &str = \"a;\n")):
        bad_file = dict(tree)
        bad_file[path] = text
        try:
            run(bad_file)
            got, v = "통과", []
        except ScanError as e:
            got, v = ("읽기 오류" if str(e).startswith(path + ":") else "경로 없는 읽기 오류"), [str(e)]
        cases.append((title, got, "읽기 오류", v))
    test_path = dict(tree)
    test_path["crates/tasty-net-rendezvous/src/lib.rs"] = "#[cfg(test)]\n#[path = \"t.rs\"]\nmod t;\n"
    test_path["crates/tasty-net-rendezvous/src/t.rs"] = "use std::process::Command;\n"
    crate_case("시험 cfg가 붙은 #[path] 시험 모듈은 검사 밖", test_path, "통과")
    leak = dict(tree)
    leak["crates/tasty-net-ssh/src/listener.rs"] = "use std::process::Stdio;\n"
    crate_case("src/system/ 밖의 금지 경로", leak, "실패")

    failed = 0
    for title, got, want, v in cases:
        if got != want:
            print(f"  ✘ {title}: 결과 {got}, 기대 {want} {v}", file=sys.stderr)
            failed += 1
    if failed:
        print(f"{TAG} 자체 시험 실패 — 사례 {len(cases)}개 중 {failed}개", file=sys.stderr)
        sys.exit(1)
    print(f"{TAG} 자체 시험 통과 — 사례 {len(cases)}개(실패해야 하는 입력 {len(bad) + 3}개)")


mode = sys.argv[1]
if mode == "self-test":
    self_test()
    sys.exit(0)


def read(p):
    with open(p, encoding="utf-8") as f:
        return f.read()


def listing(crate):
    out = []
    for d, _, names in os.walk(f"crates/{crate}/src"):
        out += [os.path.join(d, n).replace(os.sep, "/") for n in names if n.endswith(".rs")]
    return sorted(out)


try:
    violations, summary, count = collect(read, listing, lambda c: os.path.isdir(f"crates/{c}"))
except (ScanError, OSError) as e:
    print(f"{TAG} 판정 불가: {e}", file=sys.stderr)
    sys.exit(2)
if count == 0:
    print(f"{TAG} 판정 불가: 검사 대상 파일이 0개다.", file=sys.stderr)
    sys.exit(2)
if violations:
    for v in violations:
        print(f"  {v}", file=sys.stderr)
    print(f"{TAG} 위반 — {summary}", file=sys.stderr)
    print("  모바일 target에서 컴파일되는 라이브러리 코드는 다른 프로그램을 실행하지 않는다"
          "(docs/design/mobile.md \"다른 프로그램 실행 금지\").", file=sys.stderr)
    sys.exit(1)
print(f"{TAG} 통과 — {summary}")
PY
