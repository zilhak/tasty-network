#!/usr/bin/env bash
# 숨은 동작값 검사(docs/design/testing.md "설정값 시험", 동작 수준 기준 D10).
# 대상 소스에서 설정 모듈 밖의 숫자 리터럴과 Duration 상수(Duration::ZERO 등)를 모두 찾는다.
# 0과 1도 전역 예외가 아니다. 허용은 목록 파일 하나(scripts/no-hidden-constants.list)로만 한다.
# 같은 검사가 설계 문서 설정 표의 키 집합과 설정 모듈의 항목 목록을 비교한다(값 대조는
# 크레이트의 defaults_match_design_tables 시험이 한다).
#
# 사용: bash scripts/check-no-hidden-constants.sh
#       bash scripts/check-no-hidden-constants.sh --list <파일>   다른 목록 파일로 검사한다
#       bash scripts/check-no-hidden-constants.sh --self-test     판정 함수를 고정 사례로 시험한다
# 종료 코드: 통과 0, 위반(또는 자체 시험 실패) 1, 판정 불가(도구 없음·목록 오류·대상 0개) 2.
#
# 목록 파일: 한 줄에 한 항목, 칸은 탭으로 나눈다. #으로 시작하는 줄과 빈 줄은 건너뛴다.
#   target   <경로>                       검사할 파일이나 폴더(.rs 전부)
#   settings <파일>                       설정 모듈. 리터럴을 보지 않는다(기본값 표가 있는 곳)
#   table    <설정 파일> <문서> <제목>     설계 문서(docs/design/<문서>)의 <제목> 아래 첫 표의 키를
#                                         그 설정 파일의 entry("키"·fixed("키" 항목과 비교한다.
#                                         같은 설정 파일의 table 줄들은 키를 합쳐서 비교한다
#   allow    <파일> <대상> <이유>          <대상>은 다음 중 하나다
#              리터럴(예: 20, 0x2112A442)  그 파일의 그 값 리터럴 전부
#              const NAME / static NAME   그 이름의 const·static 선언 안의 리터럴
#              enum NAME                  그 이름의 enum 본문 안의 리터럴(판별값)
#              그 밖의 토큰 묶음(예: += 1)  그 토큰 순서가 나오는 자리 안의 리터럴
# 쓰이지 않은 allow 줄도 위반이다. 문자열·주석·#[cfg(test)] 항목·tests/ 폴더는 보지 않는다.

set -euo pipefail

cd "$(dirname "$0")/.."

PY=""
for c in python3 python; do
    if command -v "$c" >/dev/null 2>&1; then PY="$c"; break; fi
done
[ -n "$PY" ] || { echo "[no-hidden-constants] 판정 불가: python3가 PATH에 없다." >&2; exit 2; }

MODE=check
LIST=scripts/no-hidden-constants.list
case "${1:-}" in
    --self-test) MODE=self-test ;;
    --list)
        [ -f "${2:-}" ] || { echo "[no-hidden-constants] 판정 불가: 목록 파일이 없다: ${2:-}" >&2; exit 2; }
        LIST="$2"
        ;;
    "") ;;
    *)
        echo "[no-hidden-constants] 모르는 인자: $*" >&2
        exit 2
        ;;
esac

"$PY" - "$MODE" "$LIST" <<'PY'
import os
import re
import sys

TAG = "[no-hidden-constants]"


class ListError(Exception):
    pass


# ---------------------------------------------------------------- Rust 토큰

IDENT = re.compile(r"[A-Za-z_][A-Za-z0-9_]*")
NUMBER = re.compile(
    r"0x[0-9A-Fa-f_]+|0o[0-7_]+|0b[01_]+"
    r"|[0-9][0-9_]*(?:\.[0-9][0-9_]*)?(?:[eE][+-]?[0-9_]+)?"
)
SUFFIX = re.compile(r"(?:u8|u16|u32|u64|u128|usize|i8|i16|i32|i64|i128|isize|f32|f64)")
PUNCT3 = ("<<=", ">>=", "...", "..=")
PUNCT2 = ("::", "->", "=>", "==", "!=", "<=", ">=", "&&", "||", "+=", "-=", "*=", "/=",
          "%=", "^=", "&=", "|=", "<<", ">>", "..")
DURATION_CONSTS = {"ZERO", "MAX", "SECOND", "MILLISECOND", "MICROSECOND", "NANOSECOND"}


def tokenize(text):
    """(종류, 글자, 줄) 목록. 종류: num, ident, punct, str. 주석은 버린다."""
    out = []
    i, n, line = 0, len(text), 1
    while i < n:
        c = text[i]
        if c == "\n":
            line += 1
            i += 1
        elif c.isspace():
            i += 1
        elif text.startswith("//", i):
            j = text.find("\n", i)
            i = n if j < 0 else j
        elif text.startswith("/*", i):
            depth, j = 1, i + 2
            while j < n and depth:
                if text.startswith("/*", j):
                    depth, j = depth + 1, j + 2
                elif text.startswith("*/", j):
                    depth, j = depth - 1, j + 2
                else:
                    j += 1
            line += text.count("\n", i, j)
            i = j
        elif re.match(r"b?r#*\"", text[i:i + 8]):
            m = re.match(r"(b?r)(#*)\"", text[i:])
            close = "\"" + m.group(2)
            j = text.find(close, i + len(m.group(0)))
            j = n if j < 0 else j + len(close)
            out.append(("str", "\"\"", line))
            line += text.count("\n", i, j)
            i = j
        elif c == "\"" or text.startswith("b\"", i):
            j = i + (2 if c == "b" else 1)
            while j < n and text[j] != "\"":
                j += 2 if text[j] == "\\" else 1
            out.append(("str", "\"\"", line))
            line += text.count("\n", i, j)
            i = j + 1
        elif c == "'" or text.startswith("b'", i):
            # 문자 리터럴인지 수명('a)인지 가른다.
            j = i + (2 if c == "b" else 1)
            m = re.match(r"(?:\\(?:x[0-9A-Fa-f]{2}|u\{[0-9A-Fa-f]+\}|.)|[^\\'\n])'", text[j:])
            if m:
                out.append(("str", "''", line))
                i = j + len(m.group(0))
            else:
                m = IDENT.match(text, j)
                out.append(("ident", "'" + (m.group(0) if m else ""), line))
                i = m.end() if m else j
        elif c.isdigit():
            m = NUMBER.match(text, i)
            j = m.end()
            # 1..2 의 1. 은 소수점이 아니다.
            num = m.group(0)
            if "." in num and text.startswith(".", m.start() + num.index(".") + 1):
                num = num[:num.index(".")]
                j = m.start() + len(num)
            s = SUFFIX.match(text, j)
            if s and not (j < n and IDENT.match(text, s.end()) and text[s.end():s.end() + 1].isalnum()):
                j = s.end()
            out.append(("num", text[i:j], line))
            i = j
        elif IDENT.match(text, i):
            m = IDENT.match(text, i)
            out.append(("ident", m.group(0), line))
            i = m.end()
        else:
            for p in PUNCT3 + PUNCT2:
                if text.startswith(p, i):
                    out.append(("punct", p, line))
                    i += len(p)
                    break
            else:
                out.append(("punct", c, line))
                i += 1
    return out


def num_value(text):
    t = text.replace("_", "")
    t = re.sub(r"(?<=[0-9])(?:u8|u16|u32|u64|u128|usize|i8|i16|i32|i64|i128|isize|f32|f64)$", "", t)
    try:
        if t.lower().startswith("0x"):
            return int(t, 16)
        if t.startswith("0o"):
            return int(t[2:], 8)
        if t.startswith("0b"):
            return int(t[2:], 2)
        return int(t)
    except ValueError:
        return float(t)


def matching_close(toks, i):
    """toks[i]가 여는 괄호일 때 짝 닫는 괄호의 위치."""
    pairs = {"(": ")", "[": "]", "{": "}"}
    open_, close = toks[i][1], pairs[toks[i][1]]
    depth = 0
    for j in range(i, len(toks)):
        if toks[j][0] == "punct":
            if toks[j][1] == open_:
                depth += 1
            elif toks[j][1] == close:
                depth -= 1
                if depth == 0:
                    return j
    return len(toks) - 1


def item_end(toks, i):
    """i에서 시작하는 항목(속성 뒤)의 끝 위치: 첫 {…} 블록의 끝이나 바깥 깊이의 ;."""
    depth = 0
    for j in range(i, len(toks)):
        k, t, _ = toks[j]
        if k != "punct":
            continue
        if t in "([":
            depth += 1
        elif t in ")]":
            depth -= 1
        elif t == "{" and depth == 0:
            return matching_close(toks, j)
        elif t == "{":
            depth += 1
        elif t == "}":
            depth -= 1
        elif t == ";" and depth == 0:
            return j
    return len(toks) - 1


def strip_cfg_test(toks):
    """#[cfg(test)]가 붙은 항목을 뺀 토큰 목록."""
    out, i = [], 0
    while i < len(toks):
        if (toks[i][1] == "#" and i + 1 < len(toks) and toks[i + 1][1] == "["):
            close = matching_close(toks, i + 1)
            inner = [t[1] for t in toks[i + 2:close]]
            if inner == ["cfg", "(", "test", ")"]:
                i = item_end(toks, close + 1) + 1
                continue
        out.append(toks[i])
        i += 1
    return out


def literals(toks):
    """(위치, 글자, 줄) 목록: 숫자 리터럴과 Duration 상수. 튜플 첨자(.0)는 빼되 ..0은 본다."""
    out = []
    for i, (k, t, line) in enumerate(toks):
        if k == "num":
            if i > 0 and toks[i - 1] == ("punct", ".", toks[i - 1][2]):
                continue
            out.append((i, t, line))
        elif (k == "ident" and t in DURATION_CONSTS and i >= 2
              and toks[i - 1][1] == "::" and toks[i - 2][1] == "Duration"):
            out.append((i, "Duration::" + t, line))
    return out


def decl_spans(toks, keyword, name):
    """keyword name …의 토큰 범위 목록."""
    spans = []
    for i in range(len(toks) - 1):
        if toks[i][:2] == ("ident", keyword) and toks[i + 1][:2] == ("ident", name):
            spans.append((i, item_end(toks, i)))
    return spans


def snippet_spans(toks, snippet):
    want = [t[1] for t in tokenize(snippet)]
    spans = []
    if not want:
        return spans
    for i in range(len(toks) - len(want) + 1):
        if [t[1] for t in toks[i:i + len(want)]] == want:
            spans.append((i, i + len(want) - 1))
    return spans


# ---------------------------------------------------------------- 판정

def parse_list(text):
    entries = {"target": [], "settings": [], "table": [], "allow": []}
    for no, raw in enumerate(text.splitlines(), 1):
        if not raw.strip() or raw.lstrip().startswith("#"):
            continue
        cols = raw.split("\t")
        kind = cols[0]
        need = {"target": 2, "settings": 2, "table": 4, "allow": 4}.get(kind)
        if need is None:
            raise ListError(f"{no}행: 모르는 종류 {kind!r}")
        if len(cols) != need or not all(c.strip() for c in cols):
            raise ListError(f"{no}행: {kind} 줄은 탭으로 나눈 칸 {need}개다: {raw!r}")
        entries[kind].append((no, [c.strip() for c in cols[1:]]))
    return entries


def judge_file(path, text, allows):
    """(위반 목록, 리터럴 수). allows: [(행, 대상, 이유)]. 쓰인 allow 행을 used에 넣는다."""
    toks = strip_cfg_test(tokenize(text))
    lits = literals(toks)
    allowed_at = {}
    for no, target, _reason in allows:
        m = re.fullmatch(r"(const|static|enum)\s+([A-Za-z_][A-Za-z0-9_]*)", target)
        if m:
            spans = decl_spans(toks, m.group(1), m.group(2))
        elif re.fullmatch(r"Duration::[A-Z_]+|0x[0-9A-Fa-f_]+|[0-9][0-9_.eE]*[a-z0-9]*", target):
            want = target if target.startswith("Duration::") else num_value(target)
            for pos, t, _ in lits:
                got = t if t.startswith("Duration::") else num_value(t)
                if got == want:
                    allowed_at.setdefault(pos, no)
            continue
        else:
            spans = snippet_spans(toks, target)
        for a, b in spans:
            for pos, _, _ in lits:
                if a <= pos <= b:
                    allowed_at.setdefault(pos, no)
    used = set(allowed_at.values())
    violations = [f"{path}:{line}: {t}" for pos, t, line in lits if pos not in allowed_at]
    return violations, len(lits), used


def table_keys(doc_text, heading):
    lines = doc_text.splitlines()
    for i, l in enumerate(lines):
        if l.startswith("#") and l.lstrip("#").strip() == heading:
            break
    else:
        raise ListError(f"제목 {heading!r}이 없다")
    rows = []
    started = False
    for l in lines[i + 1:]:
        if l.startswith("|"):
            started = True
            rows.append(l)
        elif started:
            break
    if len(rows) < 3:
        raise ListError(f"{heading!r} 아래 표가 없다")
    keys = set()
    for r in rows[2:]:
        first = r.strip().strip("|").split("|")[0]
        for k in first.split(","):
            k = k.strip().strip("`")
            if k:
                keys.add(k)
    return keys


def code_keys(text):
    """설정 모듈의 entry("키"·fixed("키" 항목. 주석과 #[cfg(test)] 모듈 안은 읽지 않는다."""
    body = re.sub(r"//[^\n]*", "", text)
    m = re.search(r"#\[cfg\(test\)\]\s*mod\s", body)
    body = body[:m.start()] if m else body
    return set(re.findall(r"\b(?:entry|fixed)\(\s*\"([^\"]+)\"", body))


def collect(read, list_text, root_files):
    """판정. read(path) -> text, root_files(target) -> [path]. (위반, 요약, 대상 파일 수)."""
    entries = parse_list(list_text)
    settings = {p for _, (p,) in entries["settings"]}
    files = []
    for no, (target,) in entries["target"]:
        found = root_files(target)
        if not found:
            raise ListError(f"{no}행: 대상 {target}에 .rs 파일이 없다")
        files += [f for f in found if f not in files]
    allows_by_file = {}
    allow_files = set()
    for no, (path, target, reason) in entries["allow"]:
        allows_by_file.setdefault(path, []).append((no, target, reason))
        allow_files.add(path)
    violations, total_lits, used = [], 0, set()
    checked = [f for f in files if f not in settings and "/tests/" not in f]
    for f in checked:
        v, n, u = judge_file(f, read(f), allows_by_file.get(f, []))
        violations += v
        total_lits += n
        used |= u
    for no, (path, target, _reason) in entries["allow"]:
        if path not in checked:
            violations.append(f"목록 {no}행: {path}는 검사 대상이 아니다(허용 {target})")
        elif no not in used:
            violations.append(f"목록 {no}행: 쓰이지 않은 허용 {path} {target}")
    tables = {}
    for no, (sfile, doc, heading) in entries["table"]:
        tables.setdefault(sfile, []).append((no, doc, heading))
    key_count = 0
    for sfile, rows in tables.items():
        doc_keys = set()
        for no, doc, heading in rows:
            try:
                doc_keys |= table_keys(read(os.path.join("docs/design", doc)), heading)
            except ListError as e:
                raise ListError(f"목록 {no}행 {doc}: {e}") from None
        ck = code_keys(read(sfile))
        key_count += len(ck)
        for k in sorted(doc_keys - ck):
            violations.append(f"표 대조: 설계 표의 {k}가 {sfile}에 없다")
        for k in sorted(ck - doc_keys):
            violations.append(f"표 대조: {sfile}의 {k}가 설계 표에 없다")
    summary = (f"대상 파일 {len(checked)}개 · 리터럴 {total_lits}개 · 허용 줄 {len(entries['allow'])}개"
               f" · 설정 모듈 {len(settings)}개 · 표 대조 키 {key_count}개 · 위반 {len(violations)}개")
    return violations, summary, len(checked)


def self_test():
    hidden = (
        "use std::time::Duration;\n"
        "pub fn f() -> u32 {\n"
        "    let retries = 1;\n"
        "    let max_inflight = 1;\n"
        "    let _d = Duration::from_millis(0);\n"
        "    retries + max_inflight\n"
        "}\n"
    )
    listed = (
        "/// STUN 헤더 20 B (주석 안 숫자는 보지 않는다)\n"
        "pub const HEADER_LEN: usize = 20;\n"
        "pub enum Code { A = 0, B = 1 }\n"
        "pub fn f(v: &[u8], t: (u8, u8)) -> u8 {\n"
        "    let mut n = v[0];\n"
        "    n += 1;\n"
        "    let _s = \"retries = 1\";\n"
        "    n + t.1\n"
        "}\n"
        "#[cfg(test)]\n"
        "mod tests { fn g() -> u32 { 99 } }\n"
    )
    settings_mod = (
        "pub static DEFAULTS: &[S] = &[entry(\"a.x\", 5), fixed(\"b\", \"on\", \"on\")];\n"
    )
    doc = "# 문서\n\n## 표\n\n| 키 | 기본 |\n|---|---|\n| `a.x` | 5 |\n| `b` | on |\n"
    allow_listed = (
        "target\tsrc\n"
        "allow\tsrc/listed.rs\tconst HEADER_LEN\t와이어 형식\n"
        "allow\tsrc/listed.rs\tenum Code\t종료 코드\n"
        "allow\tsrc/listed.rs\t[0]\t첫 원소\n"
        "allow\tsrc/listed.rs\t+= 1\t세기\n"
    )
    files = {"src/hidden.rs": hidden, "src/listed.rs": listed, "src/settings.rs": settings_mod,
             "docs/design/d.md": doc}

    def run(list_text, names):
        fs = {k: v for k, v in files.items() if k in names or not k.startswith("src/")}
        return collect(lambda p: fs[p], list_text, lambda t: sorted(k for k in fs if k.startswith(t + "/")))

    cases = []

    def case(title, list_text, names, want):
        try:
            v, _, _ = run(list_text, names)
            got = len(v)
        except ListError as e:
            v, got = [str(e)], "목록 오류"
        cases.append((title, got, want, v))

    case("숨은 값(retries = 1, max_inflight = 1, Duration::from_millis(0))", "target\tsrc\n",
         {"src/hidden.rs"}, 3)
    case("목록에 오른 값만 든 파일", allow_listed, {"src/listed.rs"}, 0)
    case("허용 토큰 묶음 += 1은 retries = 1을 덮지 않는다",
         "target\tsrc\nallow\tsrc/hidden.rs\t+= 1\t세기\n", {"src/hidden.rs"}, 4)
    case("리터럴 허용 1은 그 파일의 1을 모두 덮는다",
         "target\tsrc\nallow\tsrc/hidden.rs\t1\t시험\n", {"src/hidden.rs"}, 1)
    case("쓰이지 않은 허용 줄", allow_listed + "allow\tsrc/listed.rs\t7\t없음\n", {"src/listed.rs"}, 1)
    case("설정 모듈은 보지 않고 표 키가 같으면 통과",
         "target\tsrc\nsettings\tsrc/settings.rs\ntable\tsrc/settings.rs\td.md\t표\n",
         {"src/settings.rs"}, 0)
    case("표에 없는 코드 키",
         "target\tsrc\nsettings\tsrc/settings.rs\ntable\tsrc/settings.rs\td.md\t없는 제목\n",
         {"src/settings.rs"}, "목록 오류")
    files["docs/design/d.md"] = doc.replace("| `b` | on |\n", "| `b` | on |\n| `c` | 1 |\n")
    case("설계 표에만 있는 키",
         "target\tsrc\nsettings\tsrc/settings.rs\ntable\tsrc/settings.rs\td.md\t표\n",
         {"src/settings.rs"}, 1)
    files["docs/design/d.md"] = doc
    files["src/zero.rs"] = "fn f() -> std::time::Duration { Duration::ZERO }\nfn g(r: std::ops::Range<u8>) { let _ = (r, ..2); }\n"
    case("Duration::ZERO와 ..2", "target\tsrc\n", {"src/zero.rs"}, 2)

    failed = 0
    for title, got, want, v in cases:
        if got != want:
            print(f"  ✘ {title}: 결과 {got}, 기대 {want} {v}", file=sys.stderr)
            failed += 1
    if failed:
        print(f"{TAG} 자체 시험 실패 — 사례 {len(cases)}개 중 {failed}개", file=sys.stderr)
        sys.exit(1)
    print(f"{TAG} 자체 시험 통과 — 사례 {len(cases)}개")


mode, list_path = sys.argv[1], sys.argv[2]
if mode == "self-test":
    self_test()
    sys.exit(0)


def read(p):
    with open(p, encoding="utf-8") as f:
        return f.read()


def root_files(t):
    if os.path.isfile(t):
        return [t] if t.endswith(".rs") else []
    out = []
    for d, _, names in os.walk(t):
        out += [os.path.join(d, n).replace(os.sep, "/") for n in names if n.endswith(".rs")]
    return sorted(out)


try:
    violations, summary, count = collect(read, read(list_path), root_files)
except (ListError, OSError) as e:
    print(f"{TAG} 판정 불가: {list_path}: {e}", file=sys.stderr)
    sys.exit(2)
if count == 0:
    print(f"{TAG} 판정 불가: 검사 대상 파일이 0개다({list_path}의 target 줄).", file=sys.stderr)
    sys.exit(2)
if violations:
    for v in violations:
        print(f"  {v}", file=sys.stderr)
    print(f"{TAG} 위반 — {summary}", file=sys.stderr)
    print("  동작값은 설정 모듈로 옮기고, 형식 상수·첨자는 목록에 이유와 함께 올린다"
          "(docs/design/testing.md \"설정값 시험\").", file=sys.stderr)
    sys.exit(1)
print(f"{TAG} 통과 — {summary}")
PY
