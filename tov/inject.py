#!/usr/bin/env python3
"""Link hand-written C kernels into Tov's generated C through typed contracts.

    inject.py <engine.tov> <in.c> <out.c> <hooks.h> [--shadow] [--wide] <kernel.c>...

A hook is a Tov function whose comment says `// hook:`. The comment may list
the module globals its kernel gets:

    // hook: ids of the names any token could match, into cand.
    // reads: umask tMask tLoose tStart u
    // writes: cand[:return]
    function prefilter(nt: int): int { ...reference body... }

hooks.h gets one prototype per hook, built from engine.tov and the modules it
imports (one file = one module, named or namespace relative imports followed
recursively): the Tov params, then each read, then each write. An array
`x: T[]` is `const T *x, int64_t x_len` (no const when written); a scalar is
passed by value (by pointer when written). kernels/<set>/prefilter.c defines
`k_prefilter` and is compiled on its own with hooks.h forced in, so a contract
change is a compile error. Only this script knows Tov's C names (f<m>_* and
g<m>_* per module index, p0.., tv_arr); it resolves them in the emitted C and
stops if they don't line up. A hook's `writes:` must be globals of the hook's
own module; reads may name any module's globals.

--shadow keeps each reference body as f<m>_<name>__ref and runs it and the kernel
on the same state at every call, then compares the return value and the
writes. `writes: a[:n]` compares only a[0, n), n being `return` or a written
scalar. The first difference aborts with the hook, the query and the slot.
A reference body may change only its writes, and must keep their lengths.

--wide (portable x86 builds) expects a second copy of every kernel, built for
AVX2 as k_<name>__wide, and calls it when the CPU has AVX2.

A `// hook(raw):` function's kernel is the C body itself, pasted over Tov's
(readLine: Tov has no stdin). Raw bodies may use Tov's runtime directly.

Prints the paths of the contract kernels to compile, one per line.
"""

import pathlib
import re
import sys

# Tov type -> (C type in kernels, C type in Tov's output, Tov's tv_type for arrays)
TYPES = {
    "int": ("int64_t", "tv_int", "tv_type_int"),
    "u64": ("uint64_t", "uint64_t", "tv_type_u64"),
    "u32": ("uint32_t", "uint32_t", "tv_type_u32"),
    "i32": ("int32_t", "int32_t", "tv_type_i32"),
    "u8": ("uint8_t", "uint8_t", "tv_type_u8"),
    "i8": ("int8_t", "int8_t", "tv_type_i8"),
    "bool": ("bool", "bool", "tv_type_bool"),
    "f64": ("double", "double", "tv_type_f64"),
}
TOV_SCALAR = {tov_c: name for name, (_, tov_c, _) in TYPES.items()}


def die(msg: str) -> None:
    sys.exit(f"inject: {msg}")


def ctype(t: str, where: str) -> str:
    if t not in TYPES:
        die(f"{where}: type {t} has no C mapping (add it to TYPES)")
    return TYPES[t][0]


IMPORT_RE = re.compile(r"""^import\s+(?:\{[^}]*\}|\*\s+as\s+\w+)\s+from\s*(['"])(.*?)\1""", re.M)


def modules(tov_path: str):
    """(path, source) for the entry and every module it imports, recursively."""
    seen, out = set(), []

    def visit(path, spec=None):
        path = pathlib.Path(path).resolve()
        if path in seen:
            return
        if not path.is_file():
            if spec is not None:
                die(f"{path}: imported module {spec!r} does not exist")
            die(f"{path}: module does not exist")
        seen.add(path)
        src = pathlib.Path(path).read_text()
        out.append((str(path), src))
        for m in IMPORT_RE.finditer(src):
            spec = m.group(2)
            if not spec.startswith("."):
                continue  # node:fs and friends
            p = pathlib.Path(path).parent / spec
            if p.suffix != ".tov":
                p = p.with_suffix(".tov")
            visit(p, spec)

    visit(pathlib.Path(tov_path))
    return out


def parse_tov(mods, c: str):
    """Hooks and the global types they need, from every module (cross-checked with Tov's C).

    Tov mangles module m's names to f<m>_/g<m>_: resolve each global and hook
    by regex in the emitted C, and refuse a name with 0 or >1 matches, a global
    or hook defined in two modules, or a hook writing another module's global.
    """
    declarations = {}
    for path, src in mods:
        for m in re.finditer(r"^(?:export\s+)?(?:let|const)\s+(\w+)\s*(?::\s*(\w+)(\[\])?)?\s*=", src, re.M):
            name, elem, arr = m.groups()
            if name in declarations:
                die(f"global {name} is defined in both {declarations[name]} and {path}")
            declarations[name] = path

    globals_ = {
        name: {"module": path, "c": None, "type": f"{path}: {name} has no emitted C global"}
        for name, path in declarations.items()
    }
    for path, src in mods:
        for m in re.finditer(r"^(?:export\s+)?(?:let|const)\s+(\w+)\s*(?::\s*(\w+)(\[\])?)?\s*=", src, re.M):
            name, elem, arr = m.groups()
            cms = re.findall(rf"^static (\w+) g(\d+)_{name};$", c, re.M)
            if not cms:
                continue  # inlined constant, or not a hook's business
            if len(cms) > 1:
                die(f"{path}: {name} matches {len(cms)} globals in Tov's C")
            cm = cms[0]
            globals_[name]["c"] = f"g{cm[1]}_{name}"
            # A mismatch is only an error once a hook lists the global.
            if arr:
                ok = cm[0] == "tv_arr"
                globals_[name]["type"] = (elem, True) if ok else f"{path} says {elem}[], Tov's C says {cm[0]}"
            else:
                t = TOV_SCALAR.get(cm[0])
                ok = t is not None and (not elem or elem == t)
                globals_[name]["type"] = (
                    (t, False) if ok else f"{path} says {elem or '(inferred)'}, Tov's C says {cm[0]}"
                )

    hooks = []
    fn_modules = {}
    for path, src in mods:
        for m in re.finditer(r"^(?:export\s+)?function\s+(\w+)", src, re.M):
            fn_modules.setdefault(m.group(1), []).append(path)
    for path, src in mods:
        lines = src.split("\n")
        for i, line in enumerate(lines):
            fm = re.match(r"(?:export\s+)?function (\w+)\((.*)\):\s*(\w+(?:\[\])*)\s*(?:throws \w+\s*)?\{", line)
            if not fm:
                continue
            doc, j = [], i - 1
            while j >= 0 and lines[j].startswith("//"):
                doc.insert(0, lines[j][2:].strip())
                j -= 1
            kind = next((k for k in ("hook:", "hook(raw):") for d in doc if d.startswith(k)), None)
            if not kind:
                continue
            name, params, ret = fm.groups()
            if len(fn_modules.get(name, [])) > 1:
                die(f"hook {name} is defined in both {' and '.join(fn_modules[name])}")
            ps = []
            for p in filter(None, (x.strip() for x in params.split(","))):
                pn, pt = (x.strip() for x in p.split(":"))
                ps.append((pn, pt))
            fields = {"reads": [], "writes": []}
            for d in doc:
                for f in fields:
                    if d.startswith(f + ":"):
                        fields[f] += d[len(f) + 1 :].split()
            writes = []
            for w in fields["writes"]:
                wm = re.fullmatch(r"(\w+)(?:\[:(\w+)\])?", w)
                if not wm:
                    die(f"{name}: bad writes entry {w!r} (want name or name[:return|scalar])")
                writes.append(wm.groups())
            reads = fields["reads"]
            for g in reads + [w for w, _ in writes]:
                if g not in globals_:
                    die(f"{name}: {g} is not a module global of any imported module")
                if isinstance(globals_[g]["type"], str):
                    die(f"{name}: global {g}: {globals_[g]['type']}")
            for w, _ in writes:
                if globals_[w]["module"] != path:
                    die(f"{name}: writes {w}, a global of {globals_[w]['module']}, not of {path}")
            dup = set(reads) & {w for w, _ in writes}
            if dup:
                die(f"{name}: {', '.join(sorted(dup))} in both reads and writes (writes are readable)")
            for w, n in writes:
                if n and n != "return" and (n not in globals_ or globals_[n]["type"][1] or n not in dict(writes)):
                    die(f"{name}: writes {w}[:{n}]: {n} must be `return` or a written scalar")
                if n and not globals_[w]["type"][1]:
                    die(f"{name}: writes {w}[:{n}]: {w} is not an array")
            about = doc[next(i for i, d in enumerate(doc) if d.startswith(kind)) :]
            about = [d for d in about if not d.startswith(("reads:", "writes:"))]
            hooks.append(
                {
                    "name": name,
                    "raw": kind == "hook(raw):",
                    "params": ps,
                    "ret": ret,
                    "reads": reads,
                    "writes": writes,
                    "doc": " ".join(about)[len(kind) :].strip().replace("*/", "* /"),
                }
            )
    return hooks, globals_


def check_c_signature(h, c: str):
    """Tov's C for the hook must be `static RET f<m>_name(T0 p0, T1 p1...)` exactly once."""
    ms = list(re.finditer(rf"^static ([\w ]+?) f(\d+)_{h['name']}\(([^)]*)\) \{{\n", c, re.M))
    if not ms:
        die(f"no f<m>_{h['name']} in Tov's C (did Tov change its naming?)")
    if len(ms) > 1:
        die(f"f<m>_{h['name']} matches {len(ms)} functions in Tov's C")
    m = ms[0]
    h["c"] = f"f{m.group(2)}_{h['name']}"
    if h["raw"]:
        return m
    want_ret = "void" if h["ret"] == "void" else TYPES.get(h["ret"], (None, None))[1]
    want = ", ".join(f"{TYPES.get(t, (None, t))[1]} p{i}" for i, (_, t) in enumerate(h["params"])) or "void"
    if m.group(1) != want_ret or m.group(3) != want:
        die(
            f"{h['c']} in Tov's C is `{m.group(1)} ({m.group(3)})`, the .tov implies "
            f"`{want_ret} ({want})` (did Tov change its output?)"
        )
    return m


def fields(h, g):
    """(C declaration, initializer from Tov's C) per field of k_<name>_args."""
    f = [(f"{ctype(t, h['name'])} {n}", f".{n} = p{i}") for i, (n, t) in enumerate(h["params"])]
    for r in h["reads"]:
        t, arr = g[r]["type"]
        ct = ctype(t, r)
        gn = g[r]["c"]
        f += (
            [
                (f"const {ct} *{r}", f".{r} = (const {ct} *)tv_arr_data({gn})"),
                (f"int64_t {r}_len", f".{r}_len = {gn}.len"),
            ]
            if arr
            else [(f"{ct} {r}", f".{r} = {gn}")]
        )
    for w, _ in h["writes"]:
        t, arr = g[w]["type"]
        ct = ctype(t, w)
        gn = g[w]["c"]
        f += (
            [(f"{ct} *{w}", f".{w} = ({ct} *)tv_arr_data({gn})"), (f"int64_t {w}_len", f".{w}_len = {gn}.len")]
            if arr
            else [(f"{ct} *{w}", f".{w} = &{gn}")]
        )
    return f


def prototype(h, g) -> str:
    ret = "void" if h["ret"] == "void" else ctype(h["ret"], h["name"])
    body = "".join(f"    {d};\n" for d, _ in fields(h, g))
    return f"typedef struct {{\n{body}}} k_{h['name']}_args;\n" f"{ret} k_{h['name']}(const k_{h['name']}_args *a);"


def call(h, g, wide: bool) -> str:
    inits = ", ".join(i for _, i in fields(h, g))
    k = f"k_{h['name']}"
    return f"{f'(tvk_wide ? {k}__wide : {k})' if wide else k}(&({k}_args){{{inits}}})"


def shim(h, g, sig: str, shadow: bool, wide: bool) -> str:
    name, ret = h["name"], h["ret"]
    uniq = "".join(
        f"    tvg_make_unique(&{g[w]['c']}, &{TYPES[g[w]['type'][0]][2]});\n" for w, _ in h["writes"] if g[w]["type"][1]
    )
    if not shadow:
        return f"{sig} {{\n{uniq}    {'' if ret == 'void' else 'return '}{call(h, g, wide)};\n}}\n"
    pargs = ", ".join(f"p{i}" for i in range(len(h["params"])))
    rt = "" if ret == "void" else TYPES[ret][1]
    s = [f"{sig} {{", uniq.rstrip("\n"), "    static int64_t ncall;", "    ncall++;"]
    # Each written array is held by a second reference (o_*) while the reference
    # runs. Tov copies a shared buffer before any write, so whatever the body does
    # (write in place, alias it, rebind it), o_* keeps the state from before the
    # call. Its result is copied out (a_*), then the global gets o_* back: the
    # kernel starts from that exact state, in a buffer nothing else shares.
    for w, _ in h["writes"]:
        t, arr = g[w]["type"]
        gn = g[w]["c"]
        if arr:
            s.append(f"    tv_arr o_{w} = {gn}; tv_arr_retain(o_{w});")
        else:
            s.append(f"    {TYPES[t][1]} b_{w} = {gn};")
    s.append(f"    {rt + ' r_ref = ' if rt else ''}{h['c']}__ref({pargs});")
    for w, _ in h["writes"]:
        t, arr = g[w]["type"]
        gn = g[w]["c"]
        if arr:
            s.append(
                f'    if ({gn}.len != o_{w}.len) tvk_fail("{name}", ncall, '
                f'"the reference resized {w} (%lld to %lld)", (long long)o_{w}.len, (long long){gn}.len);'
            )
            s.append(f"    void *a_{w} = tvk_dup(tv_arr_data({gn}), (size_t){gn}.len * sizeof({TYPES[t][0]}));")
            s.append(f"    tv_arr_release({gn}, &{TYPES[t][2]}); {gn} = o_{w};")
        else:
            s.append(f"    {TYPES[t][1]} a_{w} = {gn}; {gn} = b_{w};")
    s.append(f"    {rt + ' r = ' if rt else ''}{call(h, g, wide)};")
    if rt:
        s.append(
            f'    if (r != r_ref) tvk_fail("{name}", ncall, "returned %lld, the reference %lld", '
            f"(long long)r, (long long)r_ref);"
        )
    for w, n in h["writes"]:
        t, arr = g[w]["type"]
        gn = g[w]["c"]
        if not arr:
            s.append(
                f'    if ({gn} != a_{w}) tvk_fail("{name}", ncall, "{w} = %lld, the reference %lld", '
                f"(long long){gn}, (long long)a_{w});"
            )
    for w, n in h["writes"]:
        t, arr = g[w]["type"]
        gn = g[w]["c"]
        if not arr:
            continue
        ct = TYPES[t][0]
        lim = f"{gn}.len" if not n else ("r_ref" if n == "return" else f"a_{n}")
        s.append(
            f"    {{ const {ct} *k_ = tv_arr_data({gn}), *r_ = a_{w}; int64_t n_ = {lim} < {gn}.len ? {lim} : {gn}.len;"
        )
        s.append(
            f'      for (int64_t i = 0; i < n_; i++) if (k_[i] != r_[i]) tvk_fail("{name}", ncall, '
            f'"{w}[%lld] = %lld, the reference %lld", (long long)i, (long long)k_[i], (long long)r_[i]); }}'
        )
        s.append(f"    free(a_{w});")
    if rt:
        s.append("    return r;")
    s.append("}")
    return "\n".join(x for x in s if x) + "\n"


SHADOW_PRELUDE = r"""#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
static void *tvk_dup(const void *p, size_t n) {
    void *q = malloc(n ? n : 1);
    if (n) memcpy(q, p, n);
    return q;
}
static void tvk_fail(const char *hook, int64_t ncall, const char *fmt, ...) {
    va_list ap;
    va_start(ap, fmt);
    fprintf(stderr, "shadow: %s differs from its reference on query #%lld \"%s\" (call %lld): ",
            hook, (long long)tvk_query, tvk_line ? tvk_line : "", (long long)ncall);
    vfprintf(stderr, fmt, ap);
    fputc('\n', stderr);
    va_end(ap);
    abort();
}
"""


def main() -> None:
    args = sys.argv[1:]
    shadow, wide = "--shadow" in args, "--wide" in args
    args = [a for a in args if a not in ("--shadow", "--wide")]
    tov_path, src_path, out_path, header_path, *kernels = args
    mods = modules(tov_path)
    src = pathlib.Path(src_path).read_text()
    hooks, g = parse_tov(mods, src)
    by_name = {h["name"]: h for h in hooks}

    header = [
        "/* Generated by inject.py from engine.tov: one prototype per hook. Don't edit. */",
        "#pragma once",
        "#include <stdbool.h>",
        "#include <stdint.h>",
        "",
    ]
    for h in hooks:
        check_c_signature(h, src)
        if not h["raw"]:
            header += [f"/* {h['doc']} */", prototype(h, g), ""]
    pathlib.Path(header_path).write_text("\n".join(header))

    # One kernel per function: a later file of the same name wins.
    kernels = list({pathlib.Path(k).stem: k for k in kernels}.values())
    prelude, compile_ = [], []
    for k in kernels:
        name = pathlib.Path(k).stem
        h = by_name.get(name)
        if not h:
            die(f"{k}: no `// hook:` function {name} in {tov_path}")
        text = pathlib.Path(k).read_text()
        m = check_c_signature(h, src)
        end = src.index("\n}\n", m.end()) + 3
        sig = m.group(0)[:-3]
        if h["raw"]:
            brace = text.index("\n{\n") + 1 if not text.startswith("{\n") else 0
            head, body = text[:brace], text[brace:].rstrip() + "\n"
            if head.strip():
                prelude.append(f"/* {name} */\n{head.strip()}\n")
            src = src[: m.end() - 2] + body + src[end:]
        else:
            if not re.search(rf"\bk_{name}\s*\(", text):
                die(f"{k} must define k_{name}(const k_{name}_args *a) (see {header_path})")
            ref = ""
            if shadow:
                ref = src[m.start() : end].replace(f" {h['c']}(", f" {h['c']}__ref(", 1)
            src = src[: m.start()] + ref + shim(h, g, sig, shadow, wide) + src[end:]
            compile_.append(k)
            if wide:
                ret = "void" if h["ret"] == "void" else ctype(h["ret"], name)
                prelude.append(f"{ret} k_{name}__wide(const k_{name}_args *a);\n")
        print(f"inject: {name}{' (raw)' if h['raw'] else ''}", file=sys.stderr)

    # After the runtime header, so shims and raw bodies can use its types.
    at = src.index("\n", src.index("typedef struct tv_arr {")) + 1
    at = src.index("} tv_arr;\n", at) + len("} tv_arr;\n")
    glue = [
        "/* Set by the raw readLine kernel, so shadow reports can name the query. */",
        "static int64_t tvk_query;",
        "static const char *tvk_line;",
        pathlib.Path(header_path).read_text().split("#include <stdint.h>\n", 1)[1],
    ]
    if shadow:
        glue.append(SHADOW_PRELUDE)
    if wide:
        glue.append('#define tvk_wide __builtin_cpu_supports("avx2")')
    src = src[:at] + "\n" + "\n".join(glue + prelude) + src[at:]
    pathlib.Path(out_path).write_text(src)
    print("\n".join(compile_))


if __name__ == "__main__":
    main()
