#!/usr/bin/env python3
"""Copy selected lines of a thread log into a check fixture, verbatim.

Usage:

    scripts/check-fixtures.py <thread> <spec>... <out>

`<thread>` is a thread ULID (looked up under the default threads
directory) or a path to a `.jsonl` thread log.  `<spec>` is either one
`seq` or an inclusive range `from-to`; the lines whose `seq` matches are
copied **byte for byte**, in file order, never re-serialised -- the
tests parse the copied lines through the core types, and the verifier
can check the bytes against the source log.  `<out>` is the fixture to
write, and it comes last.

The script refuses to write a line matching `API_KEY`, `sk-` or `.env`,
and refuses to write anything if any selected line matches: a fixture is
a copy of a log, and a log that carries a key must not become one.
(`## Plan amendment 2` item 3.)
"""

import json
import os
import sys

DEFAULT_THREADS = os.path.expanduser("~/.local/share/aigentic/threads/aigentic")
REFUSED = ("API_KEY", "sk-", ".env")


def fail(message):
    sys.stderr.write("check-fixtures: %s\n" % message)
    raise SystemExit(1)


def resolve(thread):
    if os.sep in thread or thread.endswith(".jsonl"):
        return thread
    return os.path.join(DEFAULT_THREADS, thread + ".jsonl")


def selected(spec):
    """A spec is `N` or `N-M`; both ends are inclusive."""
    if "-" in spec:
        first, _, last = spec.partition("-")
        try:
            return int(first), int(last)
        except ValueError:
            fail("not a seq or a range: %s" % spec)
    try:
        n = int(spec)
    except ValueError:
        fail("not a seq or a range: %s" % spec)
    return n, n


def main(argv):
    if len(argv) < 4:
        fail("usage: check-fixtures.py <thread> <spec>... <out>")
    thread, specs, out = argv[1], argv[2:-1], argv[-1]
    wanted = [selected(s) for s in specs]

    try:
        with open(resolve(thread), "r", encoding="utf-8") as fh:
            lines = fh.read().splitlines()
    except OSError as err:
        fail("cannot read %s: %s" % (thread, err))

    kept = []
    for line in lines:
        try:
            seq = json.loads(line).get("seq")
        except json.JSONDecodeError:
            continue
        if seq is None or not any(first <= seq <= last for first, last in wanted):
            continue
        refused = [marker for marker in REFUSED if marker in line]
        if refused:
            fail("seq %d carries %s; refusing to copy it" % (seq, ", ".join(refused)))
        kept.append(line)

    if not kept:
        fail("no line matched %s" % " ".join(specs))

    with open(out, "w", encoding="utf-8") as fh:
        for line in kept:
            fh.write(line + "\n")
    sys.stderr.write("%s: %d lines from %s\n" % (out, len(kept), resolve(thread)))
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
