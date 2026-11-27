#!/usr/bin/env python3
"""Print the public setters and public fields of the pinned quinn-proto and rustls config types
that take a time, count, size or limit, for tests/data/dependency-setters.txt
(docs/design/testing.md "설정값 시험", dependency_setters_are_covered).

Make the rustdoc JSON first, from the repository root, with the pinned toolchain:

  RUSTC_BOOTSTRAP=1 cargo rustdoc --locked -p quinn-proto@<ver> --lib -- -Z unstable-options --output-format json
  RUSTC_BOOTSTRAP=1 cargo rustdoc --locked -p rustls@<ver> --lib -- -Z unstable-options --output-format json

then run: python3 extract_setters.py target/doc/quinn_proto.json target/doc/rustls.json
"""

import json
import sys

# The types this crate configures. Other congestion controllers' configs are left out: the
# controller is fixed to CUBIC (docs/design/p2p.md "고정값").
TYPES = {
    "quinn_proto": [
        "TransportConfig",
        "MtuDiscoveryConfig",
        "AckFrequencyConfig",
        "EndpointConfig",
        "ServerConfig",
        "ClientConfig",
        "ValidationTokenConfig",
        "CubicConfig",
        "HashedConnectionIdGenerator",
    ],
    "rustls": ["ClientConfig", "ServerConfig"],
}

# Types that carry a time, count, size or limit.
SIZED = {
    "Duration",
    "VarInt",
    "IdleTimeout",
    "u8",
    "u16",
    "u32",
    "u64",
    "usize",
    "f32",
    "f64",
}


def render(t):
    """A short text of a rustdoc JSON type."""
    if t is None:
        return "()"
    if "primitive" in t:
        return t["primitive"]
    if "resolved_path" in t:
        p = t["resolved_path"]
        name = (p.get("path") or p.get("name") or "").split("::")[-1]
        args = p.get("args") or {}
        inner = args.get("angle_bracketed", {}).get("args", [])
        parts = [render(a["type"]) for a in inner if "type" in a]
        return f"{name}<{', '.join(parts)}>" if parts else name
    if "borrowed_ref" in t:
        return "&" + render(t["borrowed_ref"]["type"])
    if "generic" in t:
        return t["generic"]
    if "array" in t:
        return f"[{render(t['array']['type'])}; {t['array']['len']}]"
    if "tuple" in t:
        return "(" + ", ".join(render(x) for x in t["tuple"]) + ")"
    if "dyn_trait" in t:
        return "dyn"
    if "impl_trait" in t:
        return "impl"
    return "?"


def sized(text):
    base = text
    for wrap in ("Option<", "Result<"):
        if base.startswith(wrap):
            base = base[len(wrap):].split(",")[0].rstrip(">")
    return base in SIZED


def main():
    out = []
    for path in sys.argv[1:]:
        doc = json.load(open(path))
        index = doc["index"]
        krate = index[str(doc["root"])]["name"]
        wanted = TYPES.get(krate, [])
        for item in index.values():
            inner = item.get("inner", {})
            if "struct" in inner and item.get("name") in wanted:
                kind = inner["struct"].get("kind", {})
                for fid in kind.get("plain", {}).get("fields", []):
                    f = index.get(str(fid))
                    if f and f.get("visibility") == "public":
                        ty = render(f["inner"]["struct_field"])
                        if sized(ty):
                            out.append(f"{krate}\tfield\t{item['name']}.{f['name']}\t{ty}")
            if "impl" not in inner:
                continue
            imp = inner["impl"]
            if imp.get("trait") is not None:
                continue
            target = imp.get("for", {}).get("resolved_path")
            if not target:
                continue
            tname = (target.get("path") or target.get("name") or "").split("::")[-1]
            if tname not in wanted:
                continue
            for mid in imp.get("items", []):
                m = index.get(str(mid))
                if not m or m.get("visibility") != "public" or "function" not in m["inner"]:
                    continue
                inputs = m["inner"]["function"]["sig"]["inputs"]
                if not inputs or inputs[0][0] != "self":
                    continue
                params = [render(t) for _, t in inputs[1:]]
                if params and any(sized(p) for p in params):
                    out.append(f"{krate}\tsetter\t{tname}::{m['name']}\t{', '.join(params)}")
    for line in sorted(set(out)):
        print(line)


main()
