"""Rust 소스를 rustc 없이 읽는 작은 도구: 토큰, 괄호 짝, 항목 끝, 시험 항목 지우기, 시험 모듈 파일.

scripts/check-no-hidden-constants.sh와 scripts/check-no-process-spawn.sh가 함께 쓴다.
"""
import os
import re

IDENT = re.compile(r"[A-Za-z_][A-Za-z0-9_]*")
NUMBER = re.compile(
    r"0x[0-9A-Fa-f_]+|0o[0-7_]+|0b[01_]+"
    r"|[0-9][0-9_]*(?:\.[0-9][0-9_]*)?(?:[eE][+-]?[0-9_]+)?"
)
SUFFIX = re.compile(r"(?:u8|u16|u32|u64|u128|usize|i8|i16|i32|i64|i128|isize|f32|f64)")
PUNCT3 = ("<<=", ">>=", "...", "..=")
PUNCT2 = ("::", "->", "=>", "==", "!=", "<=", ">=", "&&", "||", "+=", "-=", "*=", "/=",
          "%=", "^=", "&=", "|=", "<<", ">>", "..")
RAW_STR = re.compile(r"b?r(#*)\"")
RAW_IDENT = re.compile(r"r#([A-Za-z_][A-Za-z0-9_]*)")
# Rust의 엄격·예약 키워드(2018 이후). raw 식별자 r#<키워드>는 키워드가 아니므로 r#를 남긴다.
KEYWORDS = {
    "as", "async", "await", "break", "const", "continue", "crate", "dyn", "else", "enum",
    "extern", "false", "fn", "for", "if", "impl", "in", "let", "loop", "match", "mod", "move",
    "mut", "pub", "ref", "return", "self", "Self", "static", "struct", "super", "trait", "true",
    "type", "unsafe", "use", "where", "while", "abstract", "become", "box", "do", "final",
    "macro", "override", "priv", "typeof", "unsized", "virtual", "yield", "try", "gen",
}


def tokenize(text):
    """(종류, 글자, 줄) 목록. 종류: num, ident, punct, str. 주석은 버린다. raw 식별자 r#name은
    name으로 읽는다(name이 키워드면 r#name 그대로)."""
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
        elif RAW_STR.match(text, i):
            m = RAW_STR.match(text, i)
            close = "\"" + m.group(1)
            j = text.find(close, m.end())
            j = n if j < 0 else j + len(close)
            out.append(("str", text[i:j], line))
            line += text.count("\n", i, j)
            i = j
        elif c == "\"" or text.startswith("b\"", i):
            j = i + (2 if c == "b" else 1)
            while j < n and text[j] != "\"":
                j += 2 if text[j] == "\\" else 1
            out.append(("str", text[i:j + 1], line))
            line += text.count("\n", i, j)
            i = j + 1
        elif c == "'" or text.startswith("b'", i):
            # 문자 리터럴인지 수명('a)인지 가른다.
            j = i + (2 if c == "b" else 1)
            m = re.match(r"(?:\\(?:x[0-9A-Fa-f]{2}|u\{[0-9A-Fa-f]+\}|.)|[^\\'\n])'", text[j:])
            if m:
                out.append(("str", text[i:j + len(m.group(0))], line))
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
        elif RAW_IDENT.match(text, i):
            m = RAW_IDENT.match(text, i)
            name = m.group(1)
            out.append(("ident", "r#" + name if name in KEYWORDS else name, line))
            i = m.end()
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


# 바깥 깊이의 ;에서 끝나는 항목. 그 안의 {…}(제네릭 인수의 블록, 초기식)는 항목을 끝내지 않는다.
SEMI_ITEMS = {"const", "static", "type", "use", "let"}
BLOCK_ITEMS = {"fn", "mod", "struct", "enum", "union", "impl", "trait", "macro_rules"}


def item_kind(toks, i):
    for j in range(i, len(toks)):
        k, t, _ = toks[j]
        if k != "ident":
            if t in ("{", ";"):
                return None
            continue
        if t == "const" and j + 1 < len(toks) and toks[j + 1][1] == "fn":
            return "fn"
        if t in SEMI_ITEMS or t in BLOCK_ITEMS:
            return t
    return None


def item_end(toks, i):
    """i에서 시작하는 항목(속성 뒤)의 끝 위치. const·static·type·use·let은 바깥 깊이의 ;,
    그 밖은 첫 {…} 블록의 끝이나 바깥 깊이의 ;."""
    semi = item_kind(toks, i) in SEMI_ITEMS
    depth = 0
    for j in range(i, len(toks)):
        k, t, _ = toks[j]
        if k != "punct":
            continue
        if t in "([":
            depth += 1
        elif t in ")]":
            depth -= 1
        elif t == "{" and depth == 0 and not semi:
            return matching_close(toks, j)
        elif t == "{":
            depth += 1
        elif t == "}":
            depth -= 1
        elif t == ";" and depth == 0:
            return j
    return len(toks) - 1


def is_test_cfg(words):
    """속성 안의 글자 목록(#[ ] 안)이 시험에서만 켜지는 cfg인지: cfg(test), 또는 직접 인자에
    test가 있는 cfg(all(…))(예: cfg(all(test, unix))). cfg(any(test, …))는 시험 밖에서도 켜지므로
    아니다."""
    if words == ["cfg", "(", "test", ")"]:
        return True
    if words[:4] != ["cfg", "(", "all", "("] or words[-2:] != [")", ")"]:
        return False
    args, cur, depth = [], [], 0
    for w in words[4:-2]:
        if w in ("(", "[", "{"):
            depth += 1
        elif w in (")", "]", "}"):
            depth -= 1
        if w == "," and depth == 0:
            args.append(cur)
            cur = []
        else:
            cur.append(w)
    args.append(cur)
    return ["test"] in args


def strip_cfg_test(toks):
    """시험 cfg(is_test_cfg)가 붙은 항목을 뺀 토큰 목록."""
    out, i = [], 0
    while i < len(toks):
        if (toks[i][1] == "#" and i + 1 < len(toks) and toks[i + 1][1] == "["):
            close = matching_close(toks, i + 1)
            inner = [t[1] for t in toks[i + 2:close]]
            if is_test_cfg(inner):
                i = item_end(toks, close + 1) + 1
                continue
        out.append(toks[i])
        i += 1
    return out


def file_mods(text):
    """파일 안의 mod NAME; 선언 목록: (이름, 시험인지, #[path] 값 또는 None, 둘러싼 인라인 모듈
    이름들). 시험 cfg(is_test_cfg)가 붙었거나 그런 인라인 모듈(mod x { … }) 안에 있으면 시험이다."""
    toks = tokenize(text)
    out, stack, depth, i, n = [], [], 0, 0, len(toks)
    while i < n:
        is_test, path, j = False, None, i
        while j + 1 < n and toks[j][1] == "#" and toks[j + 1][1] == "[":
            close = matching_close(toks, j + 1)
            inner = toks[j + 2:close]
            words = [t[1] for t in inner]
            if is_test_cfg(words):
                is_test = True
            if len(inner) == 3 and words[:2] == ["path", "="] and inner[2][0] == "str":
                path = inner[2][1][1:-1]
            j = close + 1
        if j < n and toks[j][1] == "pub":
            j += 1
            if j < n and toks[j][1] == "(":
                j = matching_close(toks, j) + 1
        if (j + 2 < n and toks[j][:2] == ("ident", "mod") and toks[j + 1][0] == "ident"
                and toks[j + 2][1] in (";", "{")):
            name, test = toks[j + 1][1], is_test or any(t for _, t, _ in stack)
            if toks[j + 2][1] == ";":
                out.append((name, test, path, tuple(m for m, _, _ in stack)))
            else:
                depth += 1
                stack.append((name, test, depth))
            i = j + 3
            continue
        if toks[i][1] == "{":
            depth += 1
        elif toks[i][1] == "}":
            if stack and stack[-1][2] == depth:
                stack.pop()
            depth -= 1
        i += 1
    return out


def mod_file(decl, name, path, inline, known):
    """decl 파일의 mod name; 이 가리키는 파일(known 안에 있을 때만). 모듈 폴더는 lib.rs·main.rs·
    mod.rs면 그 폴더, 다른 파일은 그 파일 이름의 폴더이고, 인라인 모듈 inline 안이면 그 아래
    인라인 모듈 이름의 폴더다. #[path]는 인라인 모듈 밖이면 decl의 폴더, 안이면 모듈 폴더
    기준이고, 없으면 모듈 폴더 아래 name.rs나 name/mod.rs다."""
    d = os.path.dirname(decl)
    base = os.path.basename(decl)
    mod_dir = d if base in ("lib.rs", "main.rs", "mod.rs") else os.path.join(d, base[:-len(".rs")])
    mod_dir = os.path.join(mod_dir, *inline)
    if path is not None:
        c = os.path.normpath(os.path.join(mod_dir if inline else d, path)).replace(os.sep, "/")
        return c if c in known else None
    for c in (os.path.join(mod_dir, name + ".rs"), os.path.join(mod_dir, name, "mod.rs")):
        c = c.replace(os.sep, "/")
        if c in known:
            return c
    return None


def test_module_files(read, files):
    """시험 선언으로만 닿는 모듈 파일. 어느 선언도 가리키지 않는 파일(크레이트 뿌리 등)에서
    시험 아닌 선언만 따라 닿는 파일은 제품이고, 그 밖에 선언이 가리키는 파일은 시험이다."""
    known = set(files)
    edges, targets = {}, set()
    for f in files:
        edges[f] = []
        for name, is_test, path, inline in file_mods(read(f)):
            m = mod_file(f, name, path, inline, known)
            if m:
                edges[f].append((m, is_test))
                targets.add(m)
    product = {f for f in files if f not in targets}
    queue = list(product)
    while queue:
        for m, is_test in edges[queue.pop()]:
            if not is_test and m not in product:
                product.add(m)
                queue.append(m)
    return {f for f in files if f not in product}
