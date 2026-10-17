#!/usr/bin/env python3
"""Real-disk benchmark. Three daemons can live on this Mac: upstream fsearch
(`up`), our best build so far (`best`) and the build under test (`cand`).
Every request goes to two of them in turn (examples/e2e.rs).

  realdisk.py setup                 once: clones, Linux corpus, builds, best := this build
  realdisk.py race --self           best vs best: noise floor and flaky rows (run first)
  realdisk.py race                  best vs cand: exit 0 keep, 1 discard, 2 crash/wrong
  realdisk.py promote               cand becomes best
  realdisk.py full [--holdout]      checkpoint: up vs cand, the number we publish
  realdisk.py quick                 up vs cand, one round, for a glance
  realdisk.py score <out dir>       re-print a scoreboard from raw files
  realdisk.py holdout <seed> <out>  write a private query set
  realdisk.py stop                  stop every daemon this tool started

race/full/quick build this repo into `cand` first unless --no-build; --b BIN
uses another binary. Each verdict is appended to $FSBENCH/results.tsv.
Everything lives under $FSBENCH (default ~/fsbench; keep it short, socket
paths max 104 bytes). Loop instructions: bench/realdisk/program.md.
"""
import argparse
import hashlib
import json
import math
import os
import random
import re
import secrets
import shutil
import socket
import subprocess
import sys
import time
from collections import Counter
from pathlib import Path

REPO = Path(__file__).resolve().parents[1]
QUERIES = REPO / "bench/realdisk/queries-dev.json"
LOCK = REPO / "bench/realdisk/LOCK.sha256"
LOCKED = ["examples/e2e.rs", "tools/realdisk.py", "bench/realdisk/queries-dev.json"]
UP = ("https://github.com/noahdunnagan/fsearch", "b88f4e9a90703eb7338085b325c2852fcaf131a9")
LINUX = ("https://github.com/torvalds/linux", "a5ebb76233b79db01e82e061173cb73c6d2b5c6b")

ROOT = Path(os.environ.get("FSBENCH", Path.home() / "fsbench"))
BIN = ROOT / "bin"
# Content is only indexed under HOME, so the homes nest and the corpus sits
# inside all three: same absolute paths for every engine, no copies.
HOMES = {"up": ROOT / "homes/up", "best": ROOT / "homes/up/best", "cand": ROOT / "homes/up/best/cand"}
CORPUS = HOMES["cand"] / "corpus/linux"
LEDGER, STATE, HOLDOUT = ROOT / "results.tsv", ROOT / "state.json", ROOT / "holdout.json"
NOISE = 0.05  # default tie margin when nothing measured it
PROCS = {}
RUN = {}  # the current run's out dir and meta, for crash reports


class Crash(Exception):
    pass


def sh(*cmd, cwd=None, env=None):
    print("+", " ".join(map(str, cmd)), file=sys.stderr)
    subprocess.run(list(map(str, cmd)), cwd=cwd, env=env, check=True)


def out_of(*cmd):
    return subprocess.run(list(map(str, cmd)), capture_output=True, text=True).stdout.strip()


def sha(p):
    return hashlib.sha256(Path(p).read_bytes()).hexdigest()


def check_lock():
    want = {p: h for h, p in (l.split() for l in LOCK.read_text().splitlines())}
    bad = [f for f in LOCKED if want.get(f) != sha(REPO / f)]
    return "MODIFIED: " + ", ".join(bad) if bad else "locked"


def install(src, name):
    # Unlink first: macOS kills a signed binary rewritten in place.
    BIN.mkdir(parents=True, exist_ok=True)
    (BIN / name).unlink(missing_ok=True)
    shutil.copy2(src, BIN / name)


def setup(_):
    up = ROOT / "up"
    if not up.exists():
        sh("git", "clone", "-q", UP[0], up)
    sh("git", "-C", up, "checkout", "-q", UP[1])
    if not CORPUS.exists():
        CORPUS.mkdir(parents=True)
        sh("git", "-C", CORPUS, "init", "-q")
        sh("git", "-C", CORPUS, "fetch", "-q", "--depth", "1", LINUX[0], LINUX[1])
        sh("git", "-C", CORPUS, "checkout", "-q", "FETCH_HEAD")
    build(None)
    if not (BIN / "tov-search-best").exists():
        install(BIN / "tov-search-cand", "tov-search-best")
    if not HOLDOUT.exists():
        gen_holdout(secrets.token_hex(16), HOLDOUT)  # the seed is never shown


def build(b):
    env = dict(os.environ, RUSTFLAGS="-C target-cpu=native")
    sh("cargo", "build", "--release", "-q", "--bin", "tov-search", "--example", "e2e", cwd=REPO, env=env)
    install(REPO / "target/release/examples/e2e", "e2e")
    install(b or REPO / "target/release/tov-search", "tov-search-cand")
    if not (BIN / "tov-search-up").exists():  # pinned, built once
        sh("cargo", "build", "--release", "-q", cwd=ROOT / "up", env=env)
        install(ROOT / "up/target/release/fsearch", "tov-search-up")


def data(r):  # upstream keeps its own name
    return HOMES[r] / "Library/Application Support" / ("FSearch" if r == "up" else "TovSearch")


def sock(r):
    return data(r) / ("fsearch.sock" if r == "up" else "tov-search.sock")


def ask(r, req):
    with socket.socket(socket.AF_UNIX) as s:
        s.settimeout(10)
        s.connect(str(sock(r)))
        s.sendall(json.dumps(req).encode() + b"\n")
        return json.loads(s.makefile().readline())


def stop(_=None):
    # By binary path, so daemons a crashed run (or a `status` call) left
    # behind die too, and the user's own tov-search is never touched.
    subprocess.run(["pkill", "-f", f"{BIN}/tov-search-"], capture_output=True)
    for p in PROCS.values():
        try:
            p.wait(10)
        except subprocess.TimeoutExpired:
            pass
    PROCS.clear()
    for _ in range(100):
        if subprocess.run(["pgrep", "-f", f"{BIN}/tov-search-"], capture_output=True).returncode:
            return
        time.sleep(0.1)
    subprocess.run(["pkill", "-9", "-f", f"{BIN}/tov-search-"], capture_output=True)


def start(r, limit=900):
    """Start one daemon and wait until it answers names and has its content
    indexed (nothing pending, doc count steady 3 s). Returns ready times."""
    try:
        ask(r, {"op": "ping"})
        raise Crash(f"a daemon not started by this tool answers at {sock(r)}")
    except OSError:
        pass
    (ROOT / "run").mkdir(exist_ok=True)
    log = open(ROOT / f"run/{r}.log", "ab")
    t0 = time.time()
    PROCS[r] = subprocess.Popen([BIN / f"tov-search-{r}", "serve"], env=dict(os.environ, HOME=str(HOMES[r])),
                                stdin=subprocess.DEVNULL, stdout=log, stderr=log, start_new_session=True)
    name = since = last = None
    while time.time() - t0 < limit:
        if PROCS[r].poll() is not None:
            raise Crash(f"{r} daemon exited, see {ROOT}/run/{r}.log")
        try:
            s = ask(r, {"op": "status"})
        except (OSError, ValueError):
            s = {}
        now = time.time()
        if s.get("ok"):
            name = name or now - t0
            state = (s["content_docs"], s["content_segments"])
            if s["content_pending"] or not s["content_docs"] or state != last:
                since = None
            elif since is None:
                since = now
            elif now - since >= 3:
                return {"name_s": round(name, 2), "content_s": round(since - t0, 2), "docs": s["content_docs"], "entries": s["entries"]}
            last = state
        time.sleep(0.2)
    raise Crash(f"{r} not ready after {limit}s")


def cpu(r):
    secs = 0.0
    for part in out_of("ps", "-o", "time=", "-p", PROCS[r].pid).split(":"):
        secs = secs * 60 + float(part)
    return secs


def mem(r):
    mb = {}
    for key, n, unit in re.findall(r"(phys_footprint(?:_peak)?):\s*([\d.]+)\s*([KMG])B", out_of("footprint", PROCS[r].pid)):
        mb[key] = round(float(n) * {"K": 1 / 1024, "M": 1, "G": 1024}[unit], 1)
    return mb


def e2e(out, *args, timeout=900):
    try:
        r = subprocess.run([BIN / "e2e", *map(str, args)], capture_output=True, text=True, timeout=timeout)
    except subprocess.TimeoutExpired:
        raise Crash(f"e2e timed out after {timeout}s: {' '.join(map(str, args[:2]))}")
    with open(out / "e2e.log", "a") as f:
        f.write(f"$ e2e {' '.join(map(str, args))}\n{r.stdout}{r.stderr}\n")
    if r.returncode:
        raise Crash(f"e2e failed (a daemon died?), see {out}/e2e.log")
    for k, p in PROCS.items():
        if p.poll() is not None:
            raise Crash(f"{k} daemon exited, see {ROOT}/run/{k}.log")
    return r.stdout


def begin(a, roles):
    """Fresh out dir, harness check, build, daemons up. Returns (out, meta)."""
    q = Path(HOLDOUT if getattr(a, "holdout", False) else a.queries).resolve()
    out = ROOT / "out" / f"{time.strftime('%Y%m%d-%H%M%S')}-{a.cmd}"
    out.mkdir(parents=True)
    dirty = bool(out_of("git", "-C", REPO, "status", "--porcelain", "--", "src"))
    meta = {
        "mode": a.cmd, "A": roles[0], "B": roles[1], "harness": check_lock(), "queries": q.name, "queries_path": str(q),
        "b_commit": out_of("git", "-C", REPO, "rev-parse", "--short", "HEAD") + ("+dirty" if dirty else ""),
        "machine": " / ".join(out_of("sysctl", "-n", n) for n in ["hw.model", "machdep.cpu.brand_string", "hw.ncpu"]),
        "power": out_of("pmset", "-g", "ps").split("\n")[0], "ready": {}, "cpu_s": {},
    }
    (out / "meta.json").write_text(json.dumps(meta, indent=1))
    RUN.update(out=out, meta=meta)
    if meta["harness"] != "locked":
        raise Crash(f"harness {meta['harness']}")
    stop()
    if not a.no_build:
        build(a.b)
    if getattr(a, "self", False):
        install(BIN / "tov-search-best", "tov-search-cand")
    meta["bins"] = {r: sha(BIN / f"tov-search-{r}")[:12] for r in roles}
    if getattr(a, "crawl", False):
        for r in roles:
            shutil.rmtree(data(r), ignore_errors=True)
    for r in roles:  # one at a time, so first crawls never contend
        meta["ready"][r] = start(r)
    time.sleep(5)
    return out, meta


def finish(out, meta, mode, verdict, score, worst, code):
    (out / "meta.json").write_text(json.dumps(meta, indent=1))
    if verdict in ("discard", "crash"):  # keep the scoreboard and logs, drop bulky raw JSON
        for p in out.rglob("*.json"):
            if p.name != "meta.json":
                p.unlink()
    new = not LEDGER.exists()
    with open(LEDGER, "a") as f:
        if new:
            f.write("time\tmode\tcommit\tscore\tverdict\tworst\tqueries\tmachine\tout\n")
        f.write(f"{time.strftime('%Y-%m-%d %H:%M:%S')}\t{mode}\t{meta.get('b_commit', '')}\t{score}\t{verdict}\t{worst}"
                f"\t{meta.get('queries', '')}\t{meta.get('machine', '').split(' / ')[0]}\t{out.name}\n")
    print(f"RESULT {verdict} score={score} worst={worst} out={out}")
    sys.exit(code)


def guarded(mode, body):
    """Run body(); any crash still stops the daemons and lands in the ledger."""
    try:
        body()
    except Crash as e:
        out, meta = RUN.get("out"), RUN.get("meta", {})
        if out is None:
            out = ROOT / "out" / f"{time.strftime('%Y%m%d-%H%M%S')}-{mode}"
            out.mkdir(parents=True)
        meta["crash"] = str(e)
        print(f"crash: {e}", file=sys.stderr)
        finish(out, meta, mode, "crash", "", str(e)[:80].replace("\t", " "), 2)
    finally:
        stop()


def pair(out, meta, a, b, dual_plan, single_plan, rounds=3, flip=False, timeout=900):
    """Dual run of a vs b, then the single plan (pauses, idle: needs one
    daemon at a time) in short turns a, b, b, a, ... so drift in machine
    state (power, frequency) hits both alike. Raw files land in out as
    dual.json / gaps-A.json / gaps-B.json."""
    S = {r: f"sock:{sock(r)}" for r in HOMES}
    q = meta["queries_path"]
    c0 = {r: cpu(r) for r in (a, b)}
    e2e(out, "dual", S[a], S[b], q, out / "dual.json", *dual_plan, timeout=timeout)
    meta["cpu_s"][f"{out.name}-dual"] = {r: round(cpu(r) - c0[r], 2) for r in (a, b)}
    if not single_plan:
        return
    c0, runs = {r: cpu(r) for r in (a, b)}, {"A": [], "B": []}
    for i in range(rounds):
        for k, r in [("A", a), ("B", b)][:: -1 if (i + flip) % 2 else 1]:
            f = out / f"single-{k}.json"
            e2e(out, S[r], q, f, *single_plan, timeout=timeout)
            runs[k].append(json.loads(f.read_text()))
            f.unlink()
    meta["cpu_s"][f"{out.name}-single"] = {r: round(cpu(r) - c0[r], 2) for r in (a, b)}
    for k, maps in runs.items():
        merged = {}
        for m in maps:
            for key, v in m.items():
                if key not in merged:
                    merged[key] = v
                    continue
                merged[key]["ms"] += v["ms"]
                if v["digest"] != merged[key]["digest"] or v.get("unstable"):
                    merged[key]["unstable"] = True
        (out / f"gaps-{k}.json").write_text(json.dumps(merged))


def race(a):
    def body():
        if not a.self and not STATE.exists():
            raise Crash("no noise floor yet: run `race --self` first")
        out, meta = begin(a, ["best", "cand"])
        st = json.loads(STATE.read_text()) if STATE.exists() and not a.self else {"noise": NOISE, "cat_noise": {}, "flaky": []}
        verdicts = []
        for i in range(1 if a.self else 2):  # a keep must win two passes
            sub = out / f"pass{i}"
            sub.mkdir()
            pair(sub, meta, "best", "cand", ["name:5", "typing:3", "grep:5"], ["typing:1:120", "idle:0.5,2:3"], flip=i % 2 == 1)
            cats = load(sub)
            (sub / "scoreboard.md").write_text(board(sub.name, meta, cats, st["noise"]) + "\n")
            verdicts.append(judge(cats, st))
            print(board(f"{out.name}/{sub.name}", meta, cats, st["noise"]))
            if verdicts[-1][0] != "keep":
                break
        if a.self:
            cats = load(out / "pass0")
            r = ratios(cats)
            STATE.write_text(json.dumps({
                "noise": round(max(0.02, 2 * abs(geo(r.values()) - 1)), 4),
                "cat_noise": {c: round(max(0.10, 2 * abs(x - 1)), 4) for c, x in r.items()},
                "flaky": [k for c in cats.values() for k in c["diff"] + c["flaky"]],
                "measured": out.name}, indent=1))
            finish(out, meta, "race-self", "noise", f"{geo(r.values()):.4f}", "", 0)
        verdict, score, worst, wrong = verdicts[-1]
        meta["wrong"] = wrong
        code = {"keep": 0, "discard": 1}.get(verdict, 2)
        finish(out, meta, "race", verdict, f"{max(v[1] for v in verdicts):.4f}", worst, code)
    guarded("race", body)


def promote(_):
    install(BIN / "tov-search-cand", "tov-search-best")
    commit = out_of("git", "-C", REPO, "rev-parse", "--short", "HEAD")
    with open(LEDGER, "a") as f:
        f.write(f"{time.strftime('%Y-%m-%d %H:%M:%S')}\tpromote\t{commit}\t\tpromote\t\t\t\t{sha(BIN / 'tov-search-best')[:12]}\n")
    print(f"best := cand ({commit})")


def vs_up(a):
    full = a.cmd == "full"

    def body():
        out, meta = begin(a, ["up", "cand"])
        if full:
            pair(out, meta, "up", "cand", ["name:5", "typing:3", "grep:3"], ["typing:1:120", "idle:0.05,0.5,2,30:2"], timeout=3600)
            if a.aa:
                e2e(out, "dual", f"sock:{sock('cand')}", f"sock:{sock('cand')}", meta["queries_path"], out / "aa.json", "name:3", "grep:1")
            c0 = {r: cpu(r) for r in PROCS}
            time.sleep(60)
            meta["idle_cores"] = {r: round((cpu(r) - c0[r]) / 60, 3) for r in PROCS}
            launch = e2e(out, "launch", 40, *(f"env HOME={HOMES[r]} {BIN}/tov-search-{r} status" for r in ("up", "cand")))
            meta["launch_ms"] = dict(zip(("up", "cand"), (round(float(l.split()[0]) / 1e3, 2) for l in launch.splitlines())))
            meta["mem_mb"] = {r: mem(r) for r in PROCS}
        else:
            pair(out, meta, "up", "cand", ["name:1", "typing:1", "grep:1"], [])
        (out / "meta.json").write_text(json.dumps(meta, indent=1))
        text = score(out)
        print(text)
        r = ratios(load(out))
        worst = max(r, key=r.get) if r else ""
        finish(out, meta, a.cmd, "checkpoint" if full else "glance", f"{geo(r.values()):.4f}" if r else "", worst, 0)
    guarded(a.cmd, body)


def med(v):
    v = sorted(v)
    return v[len(v) // 2]


def geo(v):
    v = list(v)
    return math.exp(sum(math.log(max(x, 1e-3)) for x in v) / len(v)) if v else float("nan")


def compare(A, B):
    """Per "kind cat": request medians for both engines. Only requests with
    byte-identical answers count; `diff` are steady mismatches, `flaky` are
    answers that changed between rounds (live disk churn)."""
    cats = {}
    new = lambda k: cats.setdefault(k, {"n": 0, "pairs": [], "diff": [], "flaky": []})
    for key, a in A.items():
        b = B.get(key)
        if not isinstance(a, dict) or not b or a["kind"] == "typing":
            continue
        c = new(f"{a['kind']} {a['cat']}")
        c["n"] += 1
        if a.get("unstable") or b.get("unstable"):
            c["flaky"].append(key)
        elif a["digest"] != b["digest"]:
            c["diff"].append(key)
        else:
            c["pairs"].append((med(a["ms"]), med(b["ms"])))
    # A typing run's total counts only if every one of its keystrokes matched.
    for key, a in A.items():
        if isinstance(a, dict) and a["kind"] == "typing" and key in B:
            gap, last = a["cat"][3:], key.split(" ", 1)[1]
            keys = [f"key{gap} {last[:i]}" for i in range(1, len(last) + 1)]
            c = new(f"typing {a['cat']}")
            c["n"] += 1
            if all(A.get(x, {}).get("digest") == B.get(x, {}).get("digest") for x in keys):
                c["pairs"].append((med(a["ms"]), med(B[key]["ms"])))
            else:
                c["diff"].append(key)
    return cats


def load(out):
    cats = {}
    if (out / "dual.json").exists():
        d = json.loads((out / "dual.json").read_text())
        cats |= compare(d["A"], d["B"])
    if (out / "gaps-A.json").exists():
        cats |= compare(*(json.loads((out / f"gaps-{k}.json").read_text()) for k in "AB"))
    return cats


def ratios(cats):
    return {c: geo(x[1] for x in v["pairs"]) / geo(x[0] for x in v["pairs"]) for c, v in cats.items() if v["pairs"]}


def judge(cats, st):
    """keep: overall score below the noise floor and no category slower than
    its own noise; wrong: an answer differs from best's."""
    r = ratios(cats)
    s = geo(r.values())
    lim = lambda c: st["cat_noise"].get(c, 0.10)
    worst = max(r, key=lambda c: r[c] - lim(c)) if r else ""
    wrong = [k for c in cats.values() for k in c["diff"] if k not in st["flaky"]]
    if wrong:
        return "wrong", s, f"{len(wrong)} answers differ, e.g. {wrong[0]}"[:120], wrong
    if s < 1 - st["noise"] and all(x <= 1 + lim(c) for c, x in r.items()):
        return "keep", s, f"{worst} {r[worst]:.2f}" if worst else "", []
    return "discard", s, f"{worst} {r[worst]:.2f}" if worst else "", []


def board(title, meta, cats, noise, measured=False):
    A, B = meta.get("A", "A"), meta.get("B", "B")
    L = [f"# Real-disk scoreboard: {title}", ""]
    L += [f"- {k}: {meta[k]}" for k in ["harness", "queries", "b_commit", "machine", "power"] if k in meta]
    L += [f"- A = {A}, B = {B}; noise margin ±{noise:.0%} ({'measured' if measured else 'from race --self' if A == 'best' else 'default'})", "",
          "Times are ms per request: geomean of each request's median, and p90 across requests.",
          "Only requests where both engines return identical answers count.", "",
          f"| row | counted | {A} ms | {B} ms | B/A | B faster on | {A} p90 | {B} p90 | winner |",
          "|---|---|---|---|---|---|---|---|---|"]
    tally = Counter()
    p90 = lambda v: sorted(v)[int(len(v) * 0.9)]
    for c, v in sorted(cats.items()):
        p = v["pairs"]
        if not p:
            L.append(f"| {c} | 0/{v['n']} | | | | | | | none counted |")
            continue
        ga, gb = geo(x[0] for x in p), geo(x[1] for x in p)
        win = B if gb / ga < 1 - noise else A if gb / ga > 1 + noise else "tie"
        tally[win] += 1
        L.append(f"| {c} | {len(p)}/{v['n']} | {ga:.3f} | {gb:.3f} | {gb / ga:.2f} | {sum(b < a for a, b in p)}/{len(p)} "
                 f"| {p90([x[0] for x in p]):.3f} | {p90([x[1] for x in p]):.3f} | **{win}** |")
    r = ratios(cats)
    L += ["", f"**Score B/A {geo(r.values()):.3f} (geomean over rows, < 1 means B is faster). "
              f"{B} wins {tally[B]}, {A} wins {tally[A]}, ties {tally['tie']}.**", ""]
    for k, label in [("idle_cores", "background CPU while idle (cores)"), ("mem_mb", "memory (MB)"),
                     ("launch_ms", "`status` launch to exit, p50 (ms)"), ("ready", "ready after start (s)"), ("cpu_s", "daemon CPU per phase (s)")]:
        if k in meta:
            L.append(f"- {label}: `{json.dumps(meta[k])}`")
    for label, key in [("answers differ", "diff"), ("answers changed between rounds", "flaky")]:
        keys = [k for v in cats.values() for k in v[key]]
        if keys:
            L += ["", f"Set aside, {label} ({len(keys)}):"] + [f"- `{k}`" for k in keys[:20]]
    return "\n".join(L)


def score(out):
    out = Path(out)
    meta = json.loads((out / "meta.json").read_text()) if (out / "meta.json").exists() else {}
    noise, measured = NOISE, False
    if (out / "aa.json").exists():
        aa = json.loads((out / "aa.json").read_text())
        noise, measured = max([0.02] + [abs(x - 1) for x in ratios(compare(aa["A"], aa["B"])).values()]), True
    elif meta.get("A") == "best" and STATE.exists():
        noise = json.loads(STATE.read_text())["noise"]
    text = board(out.name, meta, load(out), noise, measured)
    (out / "scoreboard.md").write_text(text + "\n")
    return text


def typo(rng, t, i):
    s = list(t)
    p = rng.choice([j for j in range(1, len(s)) if s[j].isalpha()])
    if i % 4 == 0 and p + 1 < len(s) and s[p + 1].isalpha() and s[p] != s[p + 1]:
        s[p], s[p + 1] = s[p + 1], s[p]
    elif i % 4 == 1:
        del s[p]
    elif i % 4 == 2:
        s.insert(p, chr(97 + rng.randrange(26)))
    else:
        s[p] = "q" if s[p].lower() == "x" else "x"
    return "".join(s)


def gen_holdout(seed, out):
    """Same categories and sizes as the dev set, sampled from the corpus
    with a private seed, so wins can't be tuned to the queries."""
    rng = random.Random(seed)
    files = []
    for d, dirs, names in os.walk(CORPUS):
        dirs[:] = sorted(x for x in dirs if x != ".git")
        files += [(d, n) for n in sorted(names)]
    count = Counter(n for _, n in files)
    wordy = lambda n: 6 <= len(n) <= 32 and n.isascii() and " " not in n and sum(c.isalpha() for c in n) * 10 >= len(n) * 7
    targets = rng.sample([(d, n) for d, n in files if count[n] == 1 and wordy(n)], 60)
    stems = Counter(n.split(".")[0].lower() for _, n in files)
    words = rng.sample(sorted(s for s, c in stems.items() if c >= 20 and 3 <= len(s) <= 10 and s.isascii() and s.isalpha()), 30)
    tops = rng.sample(sorted(x.name for x in CORPUS.iterdir() if x.is_dir() and x.name != ".git"), 6)
    w = lambda: rng.choice(words)
    filters = ([f"ext:{e} {w()}" for e in rng.sample(["c", "h", "rst", "yaml", "S", "py", "sh", "json", "dts"], 4)]
               + [f"in:{CORPUS}/{t} {w()}" for t in tops[:4]] + [f"kind:dir {t}" for t in tops[4:]]
               + ["size:>1mb ext:c", "mtime:<1d", r"re:^[a-z]+_[0-9]+\.c$", f"{w()} !test", f"'{targets[0][1]}",
                  f"^{w()}", f"{w()}$", f"type:code {w()}", "kind:file size:>100kb", f"path:drivers.*{w()}"])
    names = ([("exact", n) for _, n in targets] + [("typo", typo(rng, n, i)) for i, (_, n) in enumerate(targets)]
             + [("word", x) for x in words] + [("multi", f"{Path(d).name.split('.')[0]} {n.split('.')[0]}") for d, n in targets[:30]]
             + [("filter", f) for f in filters])
    code = [os.path.join(d, n) for d, n in files if n.endswith((".c", ".h"))]
    ident, struct = re.compile(r"\b[a-z][a-z0-9_]{9,30}\b"), re.compile(r"^struct (\w+) \{", re.M)
    rare, syms = set(), set()
    while len(rare) < 40 or len(syms) < 5:
        body = open(rng.choice(code), errors="replace").read()
        if len(rare) < 40 and (found := ident.findall(body)):
            rare.add(rng.choice(found))
        if len(syms) < 5 and (found := struct.findall(body)):
            syms.add(rng.choice(found))
    rare = sorted(rare)
    common = rng.sample(["return", "struct", "static", "include", "unsigned", "EXPORT_SYMBOL", "kfree", "mutex_lock",
                         "printk", "TODO", "FIXME", "sizeof", "goto", "NULL", "spin_lock", "license"], 8)
    regex = rng.sample([r"static\s+int\s+\w+_probe", "TODO|FIXME|XXX", r"EXPORT_SYMBOL(_GPL)?\(\w+\)", r"#define\s+[A-Z_]{8,}",
                        r"kmalloc\(.*GFP_KERNEL", r"struct\s+\w+_ops\s*\{", "0x[0-9a-f]{8}"], 4)
    grep = ([("rare", f"grep:{x}") for x in rare] + [("common", f"grep:{x}") for x in common]
            + [("regex", f"regex:{x}") for x in regex] + [("sym", f"sym:{x}") for x in sorted(syms)]
            + [("scoped", f"in:{CORPUS}/{tops[0]} grep:{rare[0]}"), ("scoped", f"ext:h grep:{common[0]}"),
               ("scoped", f"in:{CORPUS}/{tops[1]} grep:{common[1]}"), ("scoped", f"ext:rst grep:{w()}"),
               ("scan", "in:/etc grep:root"), ("scan", "in:/usr/share/doc grep:copyright")])
    pack = lambda v: [{"cat": c, "q": q} for c, q in v]
    corpus = {"names": pack(names), "typing": [[n[:i] for i in range(1, len(n) + 1)] for _, n in targets[:10]], "grep": pack(grep)}
    Path(out).write_text(json.dumps(corpus, indent=2))
    print(f"{len(names)} name queries, 10 typing runs, {len(grep)} content queries -> {out}")


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = ap.add_subparsers(dest="cmd", required=True)
    sub.add_parser("setup").set_defaults(f=setup)
    sub.add_parser("stop").set_defaults(f=stop)
    sub.add_parser("promote").set_defaults(f=promote)
    for m, f in [("race", race), ("full", vs_up), ("quick", vs_up)]:
        p = sub.add_parser(m)
        p.add_argument("--queries", default=QUERIES)
        p.add_argument("--b", help="use this tov-search binary as cand instead of building")
        p.add_argument("--no-build", action="store_true")
        if m == "race":
            p.add_argument("--self", action="store_true", help="best vs best: measure the noise floor")
        else:
            p.add_argument("--holdout", action="store_true", help=f"use the private set at {HOLDOUT}")
            p.add_argument("--crawl", action="store_true", help="delete both indexes first and time the first crawl")
            p.add_argument("--aa", action="store_true", help="also race cand against itself to measure noise")
        p.set_defaults(f=f)
    p = sub.add_parser("score")
    p.add_argument("out")
    p.set_defaults(f=lambda a: print(score(a.out)))
    p = sub.add_parser("holdout")
    p.add_argument("seed")
    p.add_argument("out")
    p.set_defaults(f=lambda a: gen_holdout(a.seed, a.out))
    a = ap.parse_args()
    a.f(a)


if __name__ == "__main__":
    main()
