#!/usr/bin/env python3
"""Checks THIRD-PARTY-LICENSES.txt against the dependency graph, and writes
rust-license-manifest.txt. Run by generate.sh (see README.md); standard
library only.

  check.py --root ROOT --json ABOUT_JSON --document DOC --manifest OUT
           --stderr FILE... [--binary BINARY]

The distributed crates are taken from Cargo itself (cargo tree, for the GUI
binary's features and the targets in about.toml), never from cargo-about, and
cargo-about's output is checked against them. Fails (exit 1), naming every
problem, if:
  - cargo-about wrote anything to stderr (a warning: a clarification whose
    files no longer match, or one for a crate not in the graph, is only a
    warning to cargo-about)
  - the crates cargo-about covered differ from the distributed crates, or a
    version differs from Cargo.lock, or a crate is not from crates.io
  - a crate has no licence text, or a text that is not one of its own files
    (cargo-about's fallback to a canonical text, which lacks the copyright
    notice)
  - a crate is used under a licence about.toml does not accept for it, or
    under licences that do not satisfy its licence expression; or a crate's
    expression differs from its Cargo.toml without a clarification
  - a clarified file's SHA-256 differs from about.toml, or a clarification
    was not applied, or one is for a crate not in the graph
  - about.toml accepts a licence no crate is used under
  - the document does not list exactly the distributed crates, or lacks a
    licence text, or contains an absolute path of the build machine
  - the binary contains source paths of a crate that is not covered
"""

import argparse
import hashlib
import json
import os
import re
import subprocess
import sys
import tomllib

CRATES_IO = "registry+https://github.com/rust-lang/crates.io-index"
APP = "linux-image-writer"
FEATURES = "gui"
# Paths of the build environment that must never appear in the document.
HOST_PATH = re.compile(r"(^|[\s(<\"'=])/(opt|src|tmp|home|root|run|build|usr|var)/")
# Licence-like file names, searched for in a crate when matching its texts.
LICENCE_NAME = re.compile(r"^(licen[cs]e|copying|copyright|notice|unlicense|authors)", re.I)

problems = []


def fail(msg):
    problems.append(msg)
    print(f"check.py: FAIL  {msg}", file=sys.stderr)


def sha256_bytes(data):
    return hashlib.sha256(data).hexdigest()


def sha256_file(path):
    with open(path, "rb") as f:
        return sha256_bytes(f.read())


# ---- SPDX expressions ----

def spdx_tokens(expr):
    return re.findall(r"\(|\)|[A-Za-z0-9.+:-]+", expr)


def spdx_satisfied(expr, licences):
    """True if the licence expression is satisfied by the set of licences
    (each a licence id, or "id WITH exception"). AND binds tighter than OR."""
    tokens = spdx_tokens(expr)
    pos = 0

    def peek():
        return tokens[pos] if pos < len(tokens) else None

    def take():
        nonlocal pos
        pos += 1
        return tokens[pos - 1]

    def primary():
        if peek() == "(":
            take()
            value = disjunction()
            if take() != ")":
                raise ValueError(f"unbalanced parentheses in '{expr}'")
            return value
        name = take()
        if peek() == "WITH":
            take()
            name = f"{name} WITH {take()}"
        return name in licences

    def conjunction():
        value = primary()
        while peek() == "AND":
            take()
            value = primary() and value
        return value

    def disjunction():
        value = conjunction()
        while peek() == "OR":
            take()
            value = conjunction() or value
        return value

    result = disjunction()
    if pos != len(tokens):
        raise ValueError(f"cannot parse licence expression '{expr}'")
    return result


def normalized(expr):
    """A Cargo.toml licence field as an SPDX expression: Cargo's deprecated
    "MIT/Apache-2.0" means "MIT OR Apache-2.0" (as crates.io and cargo-about
    read it)."""
    return " OR ".join(part.strip() for part in expr.split("/")) if "/" in expr else expr


# ---- Cargo ----

def cargo_tree(root, target, edges):
    """{(name, version): (licence, repository, source)} of the graph."""
    out = subprocess.run(
        ["cargo", "tree", "--locked", "--offline", "--features", FEATURES,
         "--target", target, "-e", edges, "--prefix", "none",
         "-f", "{p}\t{l}\t{r}"],
        cwd=root, check=True, capture_output=True, text=True).stdout
    crates = {}
    for line in out.splitlines():
        line = line.removesuffix(" (*)")
        fields = line.split("\t")
        if len(fields) != 3:
            raise ValueError(f"unexpected cargo tree line: {line!r}")
        package, licence, repository = fields
        m = re.fullmatch(r"(\S+) v(\S+)(?: \((?!proc-macro\))([^)]*)\))?(?: \(proc-macro\))?", package)
        if not m:
            raise ValueError(f"unexpected cargo tree package: {package!r}")
        crates[(m[1], m[2])] = (licence, repository, m[3] or "")
    return crates


def union(trees):
    merged = {}
    for tree in trees:
        merged.update(tree)
    return merged


# ---- Main ----

def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--root", required=True)
    ap.add_argument("--json", required=True)
    ap.add_argument("--document", required=True)
    ap.add_argument("--manifest", required=True)
    ap.add_argument("--stderr", nargs="+", required=True)
    ap.add_argument("--binary")
    args = ap.parse_args()

    root = args.root
    here = os.path.dirname(os.path.abspath(__file__))
    about_path = os.path.join(here, "about.toml")
    template_path = os.path.join(here, "THIRD-PARTY-LICENSES.txt.hbs")
    lock_path = os.path.join(here, "cargo-about.lock")
    with open(about_path, "rb") as f:
        about = tomllib.load(f)
    with open(os.path.join(root, "Cargo.lock"), "rb") as f:
        cargo_lock = tomllib.load(f)
    with open(args.json) as f:
        report = json.load(f)
    with open(args.document, encoding="utf-8") as f:
        document = f.read()
    tool = {}
    with open(lock_path) as f:
        for line in f:
            if re.match(r"^[A-Z0-9_]+=", line):
                key, value = line.rstrip("\n").split("=", 1)
                tool[key] = value

    # cargo-about's diagnostics.
    for path in args.stderr:
        with open(path, encoding="utf-8", errors="replace") as f:
            text = f.read().strip()
        if text:
            for line in text.splitlines()[:20]:
                fail(f"cargo-about reported: {line}")

    # -- The distributed crates, from Cargo. --
    targets = about["targets"]
    trees = {kind: union(cargo_tree(root, t, kind) for t in targets) for kind in
             ("normal", "normal,no-proc-macro", "normal,build", "normal,build,dev")}
    graph = trees["normal"]
    app_keys = [k for k in graph if k[0] == APP]
    if len(app_keys) != 1:
        fail(f"expected {APP} once at the root of the graph, found {app_keys}")
    distributed = {k: v for k, v in graph.items() if k[0] != APP}
    linked = {k for k in trees["normal,no-proc-macro"] if k[0] != APP}

    lock_packages = {(p["name"], p["version"]): p for p in cargo_lock["package"]}
    for key, (_, _, path) in distributed.items():
        pkg = lock_packages.get(key)
        if pkg is None:
            fail(f"{key[0]} {key[1]}: not in Cargo.lock at this version")
        elif pkg.get("source") != CRATES_IO or not pkg.get("checksum") or path:
            fail(f"{key[0]} {key[1]}: not a crates.io crate with a checksum in Cargo.lock")

    # -- What cargo-about covered. --
    covered = {}
    for entry in report["crates"]:
        p = entry["package"]
        covered[(p["name"], p["version"])] = (entry["license"], p)
    for key in sorted(set(covered) - set(graph)):
        fail(f"{key[0]} {key[1]}: in cargo-about's output, but not a distributed crate")
    for key in sorted(set(graph) - set(covered)):
        fail(f"{key[0]} {key[1]}: a distributed crate cargo-about did not cover")
    for key in covered:
        if key[0] != APP and key in distributed and covered[key][1].get("source") != CRATES_IO:
            fail(f"{key[0]} {key[1]}: cargo-about saw it from {covered[key][1].get('source')}, not crates.io")

    # The licence texts each crate is used under (the app's own licence,
    # which the document leaves out, aside).
    def app_only(lic):
        return all(u["crate"]["name"] == APP for u in lic["used_by"])

    texts = [lic for lic in report["licenses"] if not app_only(lic)]
    used = {}  # key -> list of text indices
    for index, lic in enumerate(texts):
        if not lic.get("source_path"):
            names = ", ".join(u["crate"]["name"] for u in lic["used_by"])
            fail(f"{lic['id']} text used by {names}: cargo-about's canonical fallback, not the crate's own file")
        for u in lic["used_by"]:
            used.setdefault((u["crate"]["name"], u["crate"]["version"]), []).append(index)

    # -- Accepted licences and clarifications. --
    global_accepted = list(about["accepted"])
    crate_cfg = {k: v for k, v in about.items() if isinstance(v, dict)}
    accepted_used = {lic: 0 for lic in global_accepted}
    crate_accepted_used = {}
    clarified = {}

    for name, cfg in crate_cfg.items():
        versions = [k for k in graph if k[0] == name]
        if not versions:
            fail(f"about.toml: configuration for {name}, which is not in the graph")
            continue
        for lic in cfg.get("accepted", []):
            crate_accepted_used[(name, lic)] = 0
        clar = cfg.get("clarify")
        if clar is None:
            continue
        for key in versions:
            clarified[key] = clar
        for key in versions:
            crate_dir = os.path.dirname(covered[key][1]["manifest_path"])
            for cf in clar["files"]:
                if "start" in cf or "end" in cf:
                    fail(f"about.toml: {name}: {cf['path']}: use whole files (no start/end), so the checksum covers all of it")
                path = os.path.join(crate_dir, cf["path"])
                if not os.path.isfile(path):
                    fail(f"{name} {key[1]}: clarified file {cf['path']} does not exist")
                    continue
                actual = sha256_file(path)
                if actual != cf["checksum"]:
                    fail(f"{name} {key[1]}: {cf['path']} changed since its clarification "
                         f"(SHA-256 {actual}, about.toml {cf['checksum']}): review it, then update about.toml")

    rows = {}
    for key in sorted(distributed):
        name, version = key
        declared = distributed[key][0]
        expr = covered.get(key, ("?", {}))[0]
        indices = used.get(key, [])
        if not indices:
            fail(f"{name} {version}: no licence text")
        selected = sorted({texts[i]["id"] for i in indices})
        clar = clarified.get(key)
        if clar is not None:
            if expr != clar["license"]:
                fail(f"{name} {version}: clarification not applied (cargo-about used '{expr}')")
        elif expr != normalized(declared):
            fail(f"{name} {version}: cargo-about used '{expr}', but its Cargo.toml says '{declared}'")
        for lic in selected:
            if lic in global_accepted:
                accepted_used[lic] += 1
            elif (name, lic) in crate_accepted_used:
                crate_accepted_used[(name, lic)] += 1
            else:
                fail(f"{name} {version}: used under {lic}, which about.toml does not accept")
        try:
            if not spdx_satisfied(expr, set(selected)):
                fail(f"{name} {version}: {', '.join(selected)} do not satisfy '{expr}'")
        except ValueError as e:
            fail(f"{name} {version}: {e}")

        # Every text must be one of the crate's own files.
        crate_dir = os.path.dirname(covered[key][1]["manifest_path"]) if key in covered else None
        own = {}
        if crate_dir:
            for dirpath, dirnames, filenames in os.walk(crate_dir):
                dirnames[:] = sorted(d for d in dirnames if d not in ("target", ".git"))
                for fn in filenames:
                    if LICENCE_NAME.match(fn):
                        full = os.path.join(dirpath, fn)
                        with open(full, "rb") as f:
                            own[sha256_bytes(f.read())] = os.path.relpath(full, crate_dir)
            if clar is not None:
                for cf in clar["files"]:
                    full = os.path.join(crate_dir, cf["path"])
                    if os.path.isfile(full):
                        own[sha256_file(full)] = cf["path"]
        files = []
        for i in indices:
            digest = sha256_bytes(texts[i]["text"].encode("utf-8"))
            if digest not in own:
                fail(f"{name} {version}: its {texts[i]['id']} text is not one of its own files")
            else:
                files.append(own[digest])
        if clar is not None:
            wanted = set()
            for cf in clar["files"]:
                file_expr = cf.get("license", clar["license"])
                if any(lic in spdx_tokens(file_expr) for lic in selected):
                    wanted.add(cf["path"])
            if wanted != set(files):
                fail(f"{name} {version}: texts {sorted(files)} are not the clarified files {sorted(wanted)}")
        rows[key] = {
            "declared": declared, "expr": expr, "selected": selected,
            "texts": sorted(i + 1 for i in indices), "files": sorted(files),
            "clarified": clar is not None,
        }

    for lic, n in accepted_used.items():
        if n == 0:
            fail(f"about.toml accepts {lic}, but no crate is used under it")
    for (name, lic), n in crate_accepted_used.items():
        if n == 0 and name != APP:
            fail(f"about.toml accepts {lic} for {name}, but it is not used under it")

    # -- The document. --
    listed = re.findall(r"^(\S+) (\S+)\n    Licence: (.*)$",
                        document.split("\nCrates\n------\n", 1)[-1].split("\nLicence texts\n", 1)[0], re.M)
    listed_keys = {(n, v) for n, v, _ in listed}
    if len(listed) != len(listed_keys):
        fail("the document lists a crate twice")
    for key in sorted(listed_keys - set(distributed)):
        fail(f"the document lists {key[0]} {key[1]}, which is not a distributed crate")
    for key in sorted(set(distributed) - listed_keys):
        fail(f"the document does not list {key[0]} {key[1]}")
    for n, v, expr in listed:
        if (n, v) in rows and expr != rows[(n, v)]["expr"]:
            fail(f"the document gives {n} {v} the licence '{expr}', not '{rows[(n, v)]['expr']}'")
    shown = 0
    for lic in texts:
        users = "".join(f"    {u['crate']['name']} {u['crate']['version']}\n" for u in lic["used_by"])
        block = f"{lic['name']} ({lic['id']})\nUsed by:\n{users}{'-' * 80}\n\n{lic['text']}"
        if block not in document:
            fail(f"the document lacks the {lic['id']} text used by {lic['used_by'][0]['crate']['name']}")
        shown += 1
    if document.count("\n" + "=" * 80 + "\n") != shown:
        fail(f"the document has {document.count(chr(10) + '=' * 80 + chr(10))} licence texts, expected {shown}")
    for lineno, line in enumerate(document.splitlines(), 1):
        if HOST_PATH.search(line) or "index.crates.io-" in line:
            fail(f"the document has a build machine path at line {lineno}: {line.strip()}")

    # -- The binary: every crate whose source paths it contains is covered. --
    in_binary = set()
    if args.binary:
        with open(args.binary, "rb") as f:
            data = f.read()
        for m in re.finditer(rb"index\.crates\.io-[0-9a-f]+/([A-Za-z0-9_.+-]+)/", data):
            in_binary.add(m[1].decode())
        by_dir = {f"{n}-{v}": (n, v) for n, v in graph}
        for d in sorted(in_binary):
            if d not in by_dir or by_dir[d][0] == APP:
                fail(f"the binary contains source paths of {d}, which is not a covered crate")

    # -- Excluded crates (in Cargo.lock, not distributed). --
    excluded = []
    for key, pkg in sorted(lock_packages.items()):
        if key in graph:
            continue
        if key in trees["normal,build"]:
            reason = "build-dependency (runs on the build machine; not in the program)"
        elif key in trees["normal,build,dev"]:
            reason = "dev-dependency (tests only; not in the program)"
        else:
            reason = "not used by the GUI on Linux (other platforms, or features not enabled)"
        excluded.append((key, reason))

    if problems:
        return 1

    # -- The manifest. --
    doc_sha = sha256_file(args.document)
    roles = {k: ("linked" if k in linked else "proc-macro") for k in distributed}
    out = []
    w = out.append
    w("# Linux Image Writer Rust licence manifest (format 1)")
    w("# Written by build-aux/rust-licenses/check.py. Tab-separated, in a fixed")
    w("# order; compare two builds with diff. Records, for every crate in Cargo.lock,")
    w("# whether THIRD-PARTY-LICENSES.txt covers it, and under which licence.")
    w("")
    w("[inputs]")
    w(f"cargo-lock\t{sha256_file(os.path.join(root, 'Cargo.lock'))}")
    w(f"about-toml\t{sha256_file(about_path)}")
    w(f"template\t{sha256_file(template_path)}")
    w(f"cargo-about\t{tool['CARGO_ABOUT_VERSION']}\t{tool['CARGO_ABOUT_COMMIT']}\t{tool['CARGO_ABOUT_BINARY_SHA256']}")
    w(f"cargo\t{subprocess.run(['cargo', '--version'], check=True, capture_output=True, text=True).stdout.strip()}")
    w(f"binary\t{APP} (--features {FEATURES})")
    w(f"targets\t{' '.join(targets)}")
    w(f"accepted\t{' '.join(global_accepted)}")
    w("")
    w("[summary]")
    w(f"cargo-lock-packages\t{len(lock_packages)}")
    w(f"distributed-crates\t{len(distributed)}")
    w(f"linked\t{sum(1 for r in roles.values() if r == 'linked')}")
    w(f"proc-macro\t{sum(1 for r in roles.values() if r == 'proc-macro')}")
    w(f"excluded\t{len(excluded)}")
    w(f"clarified\t{sum(1 for r in rows.values() if r['clarified'])}")
    w(f"licence-texts\t{shown}")
    counts = {}
    for r in rows.values():
        lic = " AND ".join(r["selected"])
        counts[lic] = counts.get(lic, 0) + 1
    for lic in sorted(counts):
        w(f"used-under\t{lic}\t{counts[lic]}")
    if args.binary:
        w(f"source-paths-in-binary\t{len(in_binary)}")
    w("document\tdata/THIRD-PARTY-LICENSES.txt")
    w(f"document-sha256\t{doc_sha}")
    w("")
    w("[crates]")
    w("# role: linked (compiled into the program) or proc-macro (runs at compile")
    w("#   time; the code it generates is compiled in). in-binary: the binary contains")
    w("#   the crate's source paths (yes), or not (no; most crates leave none).")
    w("# crate\tversion\trole\tdeclared-licence\tlicence-used\tused-under\ttexts\tlicence-files\tclarified\tin-binary\tsource\tchecksum\trepository\tnotice")
    for key in sorted(distributed):
        r = rows[key]
        pkg = lock_packages[key]
        w("\t".join([
            key[0], key[1], roles[key], r["declared"], r["expr"], " AND ".join(r["selected"]),
            ",".join(str(i) for i in r["texts"]), ",".join(r["files"]),
            "yes" if r["clarified"] else "no",
            ("yes" if f"{key[0]}-{key[1]}" in in_binary else "no") if args.binary else "-",
            "crates.io", pkg["checksum"], distributed[key][1] or "-", "included"]))
    w("")
    w("[licence-texts]")
    w("# The texts in the order the document gives them.")
    w("# index\tlicence\tsha256\tused-by")
    for index, lic in enumerate(texts, 1):
        w(f"{index}\t{lic['id']}\t{sha256_bytes(lic['text'].encode('utf-8'))}\t"
          + ",".join(f"{u['crate']['name']} {u['crate']['version']}" for u in lic["used_by"]))
    w("")
    w("[clarifications]")
    w("# crate\tversion\texpression\tfile\tfile-licence\tsha256")
    for key in sorted(clarified):
        clar = clarified[key]
        for cf in clar["files"]:
            w(f"{key[0]}\t{key[1]}\t{clar['license']}\t{cf['path']}\t{cf.get('license', clar['license'])}\t{cf['checksum']}")
    w("")
    w("[excluded]")
    w("# In Cargo.lock, but not in the program: not in the document.")
    w("# crate\tversion\treason")
    for key, reason in excluded:
        w(f"{key[0]}\t{key[1]}\t{reason}")
    with open(args.manifest, "w", encoding="utf-8") as f:
        f.write("\n".join(out) + "\n")

    print(f"check.py: {len(distributed)} distributed crates ({sum(1 for r in roles.values() if r == 'linked')} linked, "
          f"{sum(1 for r in roles.values() if r == 'proc-macro')} procedural macros), each in the document with its licence text; "
          f"{len(excluded)} other Cargo.lock packages excluded")
    print(f"check.py: {sum(1 for r in rows.values() if r['clarified'])} clarified crates, their files unchanged; "
          f"{shown} licence texts, each a file of the crates using it")
    if args.binary:
        print(f"check.py: {len(in_binary)} crates' source paths found in the binary, all covered")
    print("check.py: PASS")
    return 0


if __name__ == "__main__":
    sys.exit(main())
