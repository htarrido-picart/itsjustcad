#!/usr/bin/env python3
"""Convert the iOS deck-eval manifest (Tests/Shared/OnDeviceEvalManifest.swift)
to JSON for the offline prompt-replay eval (crates/ffi/examples/deck_replay.rs).

Usage:
  python3 scripts/eval_manifest_to_json.py \
      ../itsjustcad-ios/Tests/Shared/OnDeviceEvalManifest.swift \
      > crates/ffi/tests/fixtures/deck_eval_manifest.json

Ids are `<group><index>`, exactly the tags of the iOS cloud eval
(`EvalTests/CloudDeckEvalTests.swift`): groups in `EvalPrompt.Group.allCases`
order, index within the group in `OnDeviceEvalManifest.all` order.
"""
import json
import re
import sys

src = open(sys.argv[1], encoding="utf-8").read()

group_order = []
for m in re.finditer(r"enum Group[^{]*\{(.*?)\n\s*\}", src, re.S):
    for case in re.findall(r"case ([^\n]+)", m.group(1)):
        group_order += [c.strip() for c in case.split(",")]

consts = dict(re.findall(r'static let (\w+) = "([^"]*)"', src))
all_order = re.search(r"static let all: \[EvalPrompt\] = ([^\n]+(?:\n\s+\+[^\n]+)*)", src).group(1)
arrays_in_all = re.findall(r"\w+", all_order)

def arg_list(body, key):
    m = re.search(key + r":\s*\[(.*?)\]", body, re.S)
    if not m:
        return []
    out = []
    for item in re.findall(r'"((?:[^"\\]|\\.)*)"|(\b[a-zA-Z_]\w*\b)', m.group(1)):
        if item[0] or item[0] == "" and not item[1]:
            out.append(item[0])
        else:
            out.append(consts[item[1]])
    return out

def parse_check(body):
    m = re.search(r"check:\s*\.(\w+)(\(([^)]*)\))?", body)
    kind, args = m.group(1), (m.group(3) or "")
    c = {"kind": kind}
    if kind == "count":
        c["n"] = int(args)
    elif kind == "added":
        n, *rest = [a.strip() for a in args.split(",", 1)]
        c["n"] = int(n)
        k = re.search(r'kind:\s*"([^"]*)"', args)
        c["object_kind"] = k.group(1) if k else None
    elif kind == "contains":
        c["needle"] = re.search(r'"([^"]*)"', args).group(1)
        k = re.search(r"count:\s*(\d+)", args)
        c["count"] = int(k.group(1)) if k else None
    elif kind == "changed":
        c["count"] = int(re.search(r"count:\s*(\d+)", args).group(1))
    elif kind not in ("grew", "hasMesh", "unchanged", "accepted"):
        raise SystemExit(f"unknown check {kind}")
    return c

failure_classes = dict(re.findall(
    r'"((?:[^"\\]|\\.)*)":\s*"(\w+)"',
    re.search(r"failureClasses: \[String: String\] = \[(.*?)\n\s*\]", src, re.S).group(1)))

entries = []
for name in arrays_in_all:
    block = re.search(r"static let " + name + r": \[EvalPrompt\] = \[(.*?)\n    \]", src, re.S).group(1)
    for body in re.split(r"\n\s*\.init\(", "\n" + block)[1:]:
        body = re.sub(r"//[^\n]*", "", body)
        group = re.search(r"group:\s*\.(\w+)", body).group(1)
        prompt = json.loads('"' + re.search(r'prompt:\s*"((?:[^"\\]|\\.)*)"', body).group(1) + '"')
        fc = re.search(r'failureClass:\s*"(\w+)"', body)
        entries.append({
            "group": group,
            "prompt": prompt,
            "setup": arg_list(body, "setup"),
            "verbs": arg_list(body, "verbs"),
            "check": parse_check(body),
            "require_verbs": group not in ("tuning", "heldout"),
            "failure_class": fc.group(1) if fc else failure_classes.get(prompt),
        })

out = []
for g in group_order:
    for i, e in enumerate(x for x in entries if x["group"] == g):
        out.append({"id": f"{g}{i}", **e})
assert len(out) == len(entries)
json.dump({"source": "itsjustcad-ios Tests/Shared/OnDeviceEvalManifest.swift",
           "groups": group_order, "prompts": out}, sys.stdout, indent=1, ensure_ascii=False)
sys.stdout.write("\n")
