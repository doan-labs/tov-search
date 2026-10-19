#!/usr/bin/env python3
"""Compare two engine transcripts on a typed trace or a seeded random disk."""

import argparse
import os
import random
import re
import struct
import subprocess
import sys
import tempfile
import time
from pathlib import Path


VOCAB = (
    "app",
    "archive",
    "async",
    "buffer",
    "cache",
    "clang",
    "client",
    "config",
    "controller",
    "decode",
    "dialog",
    "engine",
    "format",
    "gallery",
    "handler",
    "index",
    "kernel",
    "login",
    "module",
    "navbar",
    "network",
    "package",
    "parser",
    "promise",
    "provider",
    "query",
    "request",
    "retry",
    "router",
    "scroll",
    "search",
    "signal",
    "store",
    "subject",
    "timer",
    "token",
    "user",
    "worker",
    "café",
    "naïve",
    "résumé",
    "東京",
)
EXTENSIONS = (
    "c",
    "cc",
    "cpp",
    "css",
    "gif",
    "h",
    "html",
    "ico",
    "js",
    "json",
    "md",
    "mp4",
    "pdf",
    "png",
    "py",
    "rs",
    "sh",
    "svg",
    "toml",
    "ts",
    "tsx",
    "txt",
    "yaml",
    # A case-folded and a longer-than-7-byte extension, for the ext lists.
    "JPG",
    "MarkDown",
)
TYPE_EXTENSIONS = {
    "archive": ("zip", "tar", "gz", "tgz", "bz2", "xz", "7z", "rar", "dmg", "pkg", "iso", "zst"),
    "audio": ("mp3", "m4a", "aac", "wav", "flac", "aiff", "ogg", "opus"),
    "code": ("rs", "c", "h", "cc", "cpp", "swift", "go", "py", "js", "ts", "tsx", "java", "json", "yaml"),
    "doc": ("pdf", "doc", "docx", "txt", "md", "rtf", "odt", "pptx", "csv"),
    "font": ("ttf", "otf", "woff", "woff2"),
    "image": ("png", "jpg", "jpeg", "gif", "webp", "svg", "ico"),
    "video": ("mp4", "mov", "m4v", "mkv", "avi", "webm"),
}
TOKEN_RE = re.compile(r"[^\W_]+", re.UNICODE)
MAX_MISMATCHES = 5
RESPONSE_LIMIT = 600


def read_trace(path):
    queries = []
    for line in Path(path).read_text(encoding="utf-8").splitlines():
        query = line.rstrip()
        if query.strip() and not query.startswith("#"):
            queries.append(query)
    return queries


def expand_keystrokes(queries):
    return [query[:end] for query in queries for end in range(1, len(query) + 1)]


def engine_transcript(engine, disk, keystrokes):
    process = subprocess.Popen(
        [os.fspath(engine), os.fspath(disk)],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
    )
    try:
        ready = process.stdout.readline()
        if ready != b"ready\n":
            raise RuntimeError(f"{engine}: expected b'ready\\\\n', received {ready!r}")

        answers = []
        for query in keystrokes:
            process.stdin.write(query.encode("utf-8") + b"\n")
            process.stdin.flush()
            response = bytearray()
            while True:
                line = process.stdout.readline()
                if not line:
                    raise RuntimeError(f"{engine}: exited before the response for {query!r}")
                response.extend(line)
                if line == b"\n":
                    break
            answers.append(bytes(response))
        process.stdin.close()
        status = process.wait()
        if status:
            raise RuntimeError(f"{engine}: exited with status {status}")
        return answers
    finally:
        if process.poll() is None:
            process.kill()
            process.wait()


def show_response(response):
    text = response[:RESPONSE_LIMIT].decode("utf-8", errors="replace")
    if len(response) > RESPONSE_LIMIT:
        text += f"... ({len(response) - RESPONSE_LIMIT} more bytes)"
    return repr(text)


def compare(engine_a, engine_b, disk, keystrokes, label):
    answers_a = engine_transcript(engine_a, disk, keystrokes)
    answers_b = engine_transcript(engine_b, disk, keystrokes)
    mismatches = [i for i, (a, b) in enumerate(zip(answers_a, answers_b)) if a != b]
    print(f"{label}: {len(keystrokes)} keystrokes, {len(mismatches)} mismatches")
    for i in mismatches[:MAX_MISMATCHES]:
        print(f"  keystroke {i + 1}, query={keystrokes[i]!r}")
        print(f"    A: {show_response(answers_a[i])}")
        print(f"    B: {show_response(answers_b[i])}")
    if len(mismatches) > MAX_MISMATCHES:
        print(f"  ... {len(mismatches) - MAX_MISMATCHES} more mismatches")
    return 1 if mismatches else 0


def push_record(records, children, parent, name, kind, size, mtime):
    if b"/" in name or any(c in name for c in (b"\t", b"\n", b"\r")):
        raise ValueError(f"invalid entry name {name!r}")
    if name in children.setdefault(parent, set()):
        raise ValueError(f"duplicate entry name {name!r}")
    children[parent].add(name)
    flags = kind | (4 if name.startswith(b".") else 0)
    records.append((parent, flags, size, mtime, name))
    children[len(records) - 1] = set() if kind == 1 else None
    return len(records) - 1


def mixed_case(rng, word):
    style = rng.randrange(4)
    if style == 0:
        return word.lower()
    if style == 1:
        return word.upper()
    if style == 2:
        return word.title()
    return "".join(c.upper() if rng.randrange(2) else c.lower() for c in word)


def random_name(rng, directory=False):
    separator = rng.choice("._-")
    parts = [mixed_case(rng, rng.choice(VOCAB))]
    if rng.random() < 0.7:
        parts.append(mixed_case(rng, rng.choice(VOCAB)))
    parts.append("".join(rng.choice("abcdefghijklmnopqrstuvwxyz0123456789") for _ in range(rng.randint(2, 6))))
    name = separator.join(parts)
    if rng.random() < 0.1:
        name += "".join(rng.choice("abcdefghijklmnopqrstuvwxyz0123456789") for _ in range(rng.randint(50, 90)))
    if rng.random() < 0.06:
        name = "." + name
    if not directory:
        name += "." + rng.choice(EXTENSIONS)
    return name


def random_disk(path, seed):
    rng = random.Random(seed)
    now = int(time.time())
    target = rng.randint(20_000, 200_000)
    records = [(0, 1, 0, 0, b"")]
    children = {0: set()}
    users = push_record(records, children, 0, b"Users", 1, 0, now)
    applications = push_record(records, children, 0, b"Applications", 1, 0, now)
    home = push_record(records, children, users, b"bench", 1, 0, now)
    corpus_name = f"random-{seed}".encode()
    corpus = push_record(records, children, home, corpus_name, 1, 0, now)
    corpus_path = [corpus_name.decode()]
    directories = [(corpus, corpus_path, 0)]
    files = []
    push_record(records, children, users, b"a", 0, 1, now)
    files.append(("a", "a", 0, ["Users"], 1))
    push_record(records, children, applications, b"a", 0, 1, now - 365 * 86_400)
    files.append(("a", "a", 0, ["Applications"], 1))
    push_record(records, children, users, b"abc.xyz", 0, 1, now)
    files.append(("abc.xyz", "xyz", 0, ["Users"], 1))
    push_record(records, children, users, b".abc", 0, 1, now)
    hidden_dir = push_record(records, children, users, b".hidden", 1, 0, now)
    support_dir = push_record(records, children, hidden_dir, b"Application Support", 1, 0, now)
    push_record(records, children, support_dir, b".abc.app", 0, 1, now)
    root_hidden_dir = push_record(records, children, 0, b".hidden", 1, 0, now)
    push_record(records, children, root_hidden_dir, b"xyz.app", 0, 1, now)
    applications_hidden = push_record(records, children, applications, b".hidden", 1, 0, now)
    push_record(records, children, applications_hidden, b"xyz.app", 1, 0, now)
    push_record(records, children, users, b".z", 0, 1, now)
    for number in range(64):
        push_record(records, children, applications, f"a{number:03d}.app".encode(), 1, 0, now)
    high_prior_40 = push_record(records, children, applications, b"Applications", 1, 0, now)
    high_prior_60 = push_record(records, children, high_prior_40, b"Applications", 1, 0, now)
    high_prior_60 = push_record(records, children, high_prior_60, b"Applications", 1, 0, now)
    high_prior_50 = push_record(records, children, high_prior_60, b"folders", 1, 0, now)
    push_record(records, children, high_prior_50, b"abcmax", 0, 1, now)
    push_record(records, children, high_prior_60, b"abcmax", 0, 1, now - 400 * 86_400)
    push_record(records, children, high_prior_40, b"abcxyz", 0, 1, now - 366 * 86_400)
    for i in range(64):
        push_record(records, children, high_prior_40, f"abcfix{i:02d}".encode(), 0, 1, now)
    push_record(records, children, high_prior_40, b"zxxxx", 0, 1, now - 366 * 86_400)
    push_record(records, children, high_prior_60, b"abc123", 0, 1, now - 366 * 86_400)
    for i in range(64):
        push_record(records, children, high_prior_60, f"xyz{i:03d}".encode(), 0, 1, now)
    high_prior_70 = push_record(records, children, high_prior_60, b"Applications", 1, 0, now)
    for i in range(64):
        name = b"xyz" + b"x" * 29 + f"{i:04d}".encode()
        push_record(records, children, high_prior_70, name, 0, 1, now)
    qz_prior_30 = push_record(records, children, high_prior_40, b"folders", 1, 0, now)
    qz_prior_35 = push_record(records, children, high_prior_40, b"Application Support", 1, 0, now)
    push_record(records, children, qz_prior_30, b"qz0000", 0, 1, now)
    for i in range(1, 51):
        name = f"qz{i:04d}".encode()
        push_record(records, children, qz_prior_30, name, 0, 1, now)
        push_record(records, children, qz_prior_35, name, 0, 1, now - 400 * 86_400)
    for i in range(14):
        name = f"qza{i:02d}".encode()
        push_record(records, children, qz_prior_30, name, 0, 1, now - 3 * 86_400)
    low_dir = push_record(records, children, corpus, b"a-low", 1, 0, now)
    high_a_dir = push_record(records, children, corpus, b"b-high-a", 1, 0, now)
    high_z_dir = push_record(records, children, corpus, b"c-high-z", 1, 0, now)
    bulk_dir = push_record(records, children, corpus, b"z-bulk", 1, 0, now)
    low_path = corpus_path + ["a-low"]
    high_a_path = corpus_path + ["b-high-a"]
    tie_name = b"equal-stat-ties"
    tie_parent = push_record(records, children, corpus, tie_name, 1, 0, now)
    tie_path = corpus_path + [tie_name.decode()]
    directories.append((tie_parent, tie_path, 1))
    fixed_files = (
        ("a", low_dir, low_path),
        ("ab", low_dir, low_path),
        ("abc", low_dir, low_path),
        ("abcd", low_dir, low_path),
        ("a.c", low_dir, low_path),
        ("ab.dat", low_dir, low_path),
        ("abc.app", high_a_dir, high_a_path),
        ("alphabet.app", high_a_dir, high_a_path),
        (".alpha.dat", low_dir, low_path),
        ("rank", high_a_dir, high_a_path),
        ("sharedprior.dat", low_dir, low_path),
        ("sharedprior.dat", high_a_dir, high_a_path),
    )
    for name, parent, parts in fixed_files:
        encoded = name.encode("ascii")
        if encoded not in children[parent]:
            push_record(records, children, parent, encoded, 0, 1, now)
            files.append((name, name.rsplit(".", 1)[-1], 0, parts, 1))
    ancestor_name = b"ancestor"
    ancestor_dir = push_record(records, children, corpus, ancestor_name, 1, 0, now)
    ancestor_path = corpus_path + [ancestor_name.decode("ascii")]
    directories.append((ancestor_dir, ancestor_path, 1))
    child_name = b"plainchild.dat"
    push_record(records, children, ancestor_dir, child_name, 0, 1, now)
    files.append(("plainchild.dat", "dat", 0, ancestor_path, 1))
    for number in range(100, 151):
        name = f"aa{number:03d}.dat"
        push_record(records, children, low_dir, name.encode("ascii"), 0, 1, now - 365 * 86_400)
        files.append((name, "dat", 0, low_path, 1))
    push_record(records, children, high_a_dir, b"aa000.dat", 0, 1, now)
    push_record(records, children, high_a_dir, b"aa150.dat", 0, 1, now)
    for number in range(100, 150):
        push_record(records, children, high_z_dir, f"aa{number:03d}.dat".encode("ascii"), 0, 1, now)
    tie_count = min(target - len(records), max(4096, target // 4))
    for _ in range(tie_count):
        name = random_name(rng).lstrip(".")
        encoded = name.encode("utf-8")
        if encoded in children[tie_parent]:
            continue
        ext = encoded.rsplit(b".", 1)[-1].decode("ascii", errors="ignore").lower()
        size = rng.choice((0, 1, 32, 512, 4096, 65536, 1_048_576, rng.randrange(1_000_000)))
        push_record(records, children, tie_parent, encoded, 0, size, now)
        files.append((name, ext, 0, tie_path, size))

    bulk_count = min(60_000, target - len(records))
    for index in range(bulk_count):
        push_record(records, children, bulk_dir, f"aa{index:04x}.dat".encode("ascii"), 0, 1, now - 365 * 86_400)

    while len(records) < target:
        parent, parts, depth = rng.choice(directories)
        if depth < 4 and len(records) < target - 1 and rng.random() < 0.025:
            name = random_name(rng, directory=True).encode("utf-8")
            if name not in children[parent]:
                child = push_record(records, children, parent, name, 1, 0, now - rng.randrange(1_000_000))
                directories.append((child, parts + [name.decode("utf-8")], depth + 1))
            continue

        name = random_name(rng).encode("utf-8")
        if name in children[parent]:
            continue
        ext = name.rsplit(b".", 1)[-1].decode("ascii", errors="ignore").lower()
        kind = 2 if rng.random() < 0.035 else 0
        size = 0 if kind == 2 else rng.choice((0, 1, 32, 512, 4096, 65536, 1_048_576, rng.randrange(1_000_000)))
        age = rng.choice(
            (0, 1, 60, 3600, 86_400, 3 * 86_400, 30 * 86_400, 365 * 86_400, rng.randrange(6 * 365 * 86_400))
        )
        push_record(records, children, parent, name, kind, size, now - age)
        files.append((name.decode("utf-8"), ext, kind, parts, size))

    with open(path, "wb") as disk:
        disk.write(b"FSDISK01")
        disk.write(struct.pack("<QI", len(records), now))
        for parent, kind, size, mtime, name in records:
            disk.write(struct.pack("<IBQIH", parent, kind, size, mtime, len(name)))
            disk.write(name)
    return len(records), files, directories, tie_count, bulk_count


def typo(rng, token):
    if len(token) < 5:
        token += rng.choice("abcdefghijklmnopqrstuvwxyz0123456789")
    op = rng.randrange(4)
    if op == 0:
        swappable = [i for i in range(len(token) - 1) if token[i] != token[i + 1]]
        if not swappable:
            op = 3
        else:
            at = rng.choice(swappable)
            return token[:at] + token[at + 1] + token[at] + token[at + 2 :]
    at = rng.randrange(len(token) - 1)
    if op == 1:
        return token[:at] + token[at + 1 :]
    if op == 2:
        return token[:at] + rng.choice("abcdefghijklmnopqrstuvwxyz0123456789") + token[at:]
    replacement = rng.choice("abcdefghijklmnopqrstuvwxyz0123456789")
    while replacement == token[at].lower():
        replacement = rng.choice("abcdefghijklmnopqrstuvwxyz0123456789")
    return token[:at] + replacement + token[at + 1 :]


def random_queries(rng, files, directories, count):
    queries = []
    kinds = [
        "prefix",
        "substring",
        "typo",
        "multi",
        "negative",
        "exact",
        "suffix",
        "ext",
        "type",
        "kind",
        "size",
        "scope",
        "mixed",
        "neg-only",
        "neg-pair",
        "ext-only",
        "kind-only",
        "size-only",
        "type-only",
        "filter-neg",
        "scope-neg",
        "short-one",
        "short-two",
        "short-filter-one",
        "short-filter-two",
        "tie-probe",
    ]
    rng.shuffle(kinds)
    for index in range(count):
        name, ext, kind, parts, size = rng.choice(files)
        tokens = TOKEN_RE.findall(name)
        if not tokens:
            tokens = [name]
        token = rng.choice(tokens)
        short_tokens = [part for part in tokens if len(part) >= 2]
        short_token = rng.choice(short_tokens or tokens)
        mode = kinds[index % len(kinds)]
        other = rng.choice(TOKEN_RE.findall(rng.choice(files)[0]) or tokens)
        if mode == "prefix":
            text = f"^{token[:rng.randint(1, len(token))]}"
        elif mode == "substring":
            start = rng.randrange(len(token))
            text = token[start : start + rng.randint(1, max(1, len(token) - start))]
        elif mode == "short-one":
            text = short_token[:1]
        elif mode == "short-two":
            text = short_token[:2]
        elif mode == "short-filter-one":
            text = f"ext:{ext} {short_token[:1]}"
        elif mode == "short-filter-two":
            text = f"ext:{ext} {short_token[:2]}"
        elif mode == "tie-probe":
            text = "aa"
        elif mode == "typo":
            text = typo(rng, token)
        elif mode == "multi":
            text = f"{token} {other}"
        elif mode == "negative":
            text = f"!{other} {token}"
        elif mode == "neg-only":
            text = f"!{token}"
        elif mode == "neg-pair":
            text = f"!{token} !{other}"
        elif mode == "exact":
            text = "'" + token
        elif mode == "suffix":
            text = token[-rng.randint(1, len(token)) :] + "$"
        elif mode == "ext":
            text = f"ext:{ext} {token}"
        elif mode == "type":
            file_type = rng.choice(tuple(TYPE_EXTENSIONS))
            selected = rng.choice(TYPE_EXTENSIONS[file_type])
            text = f"type:{file_type} ext:{selected} {token}"
        elif mode == "kind":
            text = f"kind:{rng.choice(('file', 'dir', 'link'))} {token}"
        elif mode == "size":
            text = f"size:{rng.choice(('>1kb', '<1mb', '1..65536'))} {token}"
        elif mode == "scope":
            directory = rng.choice(directories)
            scope = "/".join(directory[1])
            text = f"in:~/{scope} {token}"
        elif mode == "mixed":
            directory = rng.choice(directories)
            scope = "/".join(directory[1])
            text = f"in:~/{scope} ext:{ext} ^{token[:rng.randint(1, len(token))]}"
        elif mode == "ext-only":
            text = f"ext:{ext}"
        elif mode == "kind-only":
            text = "kind:dir"
        elif mode == "size-only":
            text = "size:>1kb"
        elif mode == "type-only":
            text = "type:image"
        elif mode == "filter-neg":
            text = f"ext:{ext} !{other}"
        else:
            scope = "/".join(directories[1][1])
            text = f"in:~/{scope} !{other}"
        queries.append(text)
    corpus_scope = "/".join(directories[0][1])
    ancestor_parts = next(parts for _, parts, _ in directories if parts[-1] == "ancestor")
    ancestor_scope = "/".join(ancestor_parts)
    queries.extend(
        (
            "a",
            "ab",
            "abc",
            "xyz",
            "qz",
            "z",
            "abcd",
            "ab",
            "app",
            "alpha",
            "aa",
            "sharedprior",
            "rank",
            "ran",
            "ancestor plainchild",
            f"in:~/{ancestor_scope} ancestor plainchild",
            f"in:~/{corpus_scope} app",
        )
    )
    return queries


def has_positive_token(query):
    filters = ("ext:", "kind:", "size:", "type:", "in:")
    return any(not word.startswith(("!", *filters)) for word in query.split())


def trace_mode(argv):
    parser = argparse.ArgumentParser(prog="diff.py")
    parser.add_argument("engineA")
    parser.add_argument("engineB")
    parser.add_argument("--disk", required=True)
    parser.add_argument("--trace", required=True)
    args = parser.parse_args(argv)
    queries = read_trace(args.trace)
    keystrokes = expand_keystrokes(queries)
    return compare(args.engineA, args.engineB, args.disk, keystrokes, Path(args.trace).name)


def random_mode(argv):
    parser = argparse.ArgumentParser(prog="diff.py random")
    parser.add_argument("engineA")
    parser.add_argument("engineB")
    parser.add_argument("--seed", type=int, required=True)
    parser.add_argument("--count", type=int, default=100, help="number of full random queries (default: 100)")
    args = parser.parse_args(argv)
    if args.count < 1:
        parser.error("--count must be positive")
    rng = random.Random(args.seed)
    with tempfile.TemporaryDirectory(prefix="tov-diff-") as tmp:
        disk = Path(tmp) / "random.bin"
        entries, files, directories, ties, bulk_count = random_disk(disk, args.seed)
        queries = random_queries(rng, files, directories, args.count)
        keystrokes = expand_keystrokes(queries)
        keystrokes.extend(("a", "ab", "abc", "abcd", "abc"))
        no_positive = sum(not has_positive_token(query) for query in queries)
        print(
            f"random disk: seed={args.seed}, entries={entries}, equal-stat ties={ties}, "
            f"byname-aa cohort={bulk_count + 103}, full queries={len(queries)}, no-positive={no_positive}"
        )
        return compare(args.engineA, args.engineB, disk, keystrokes, f"random seed {args.seed}")


def main(argv):
    try:
        if argv and argv[0] == "random":
            return random_mode(argv[1:])
        return trace_mode(argv)
    except (OSError, RuntimeError, ValueError) as error:
        print(f"diff.py: {error}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
