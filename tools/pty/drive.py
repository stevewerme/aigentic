#!/usr/bin/env python3
"""Drive a terminal program in a pty from a step file.

The driver runs `--bin` in a fresh pty whose `HOME`, `XDG_*` and `TMPDIR`
point into a scratch tree under the temp dir, types the step file's
keystrokes, renders the screen with pyte, and writes the dumps.

It refuses every path that could reach a real project, a real config or a
`.env` above the child's cwd, because the program it drives writes threads,
history and `.aigentic/` state wherever it is started. `README.md` next to
this file is the user-facing page.
"""

import argparse
import errno
import fcntl
import importlib
import importlib.metadata
import os
import pty
import re
import select
import shutil
import signal
import struct
import sys
import tempfile
import termios
import time

PROG = "drive.py"

KEYS = {
    "enter": b"\r",
    "tab": b"\t",
    "esc": b"\x1b",
    "up": b"\x1b[A",
    "down": b"\x1b[B",
    "C-c": b"\x03",
    "C-d": b"\x04",
}

VERBS = ("send", "key", "wait", "sleep", "dump")
DUMP_NAME = re.compile(r"^[A-Za-z0-9_-]+$")
THREADS_DIR = re.compile(r"^\s*threads_dir\s*=")
CROCKFORD = "0123456789ABCDEFGHJKMNPQRSTVWXYZ"

# The tui's in-process daemon puts its socket at <TMPDIR>/aigentic-<ulid>/
# aigentic.sock, and a unix socket path is capped at about 103 bytes, so the
# child's TMPDIR has to stay short.
SOCKET_TAIL = len("/aigentic-" + "0" * 26 + "/aigentic.sock")
SOCKET_LIMIT = 103
SHORT_TMP_ROOT = "/tmp"


class Failure(Exception):
    """A step failed or the child did not exit cleanly; the run exits 1."""


def refuse(message):
    """Report a refused argument or script and exit 2."""
    sys.stderr.write("%s: %s\n" % (PROG, message))
    raise SystemExit(2)


def fail(message):
    """Report a failed run on stderr; the caller exits 1."""
    sys.stderr.write("%s: %s\n" % (PROG, message))


def under(path, parent):
    """True when the resolved path lies strictly inside the resolved parent."""
    return path != parent and os.path.commonpath([path, parent]) == parent


def random_name():
    """8 characters of randomness: a short directory name for the child's temp,
    and two runs at once never take the same one."""
    return "".join(CROCKFORD[byte & 0x1F] for byte in os.urandom(8))


def ulid():
    """A 26-character ULID, from the clock and `os.urandom`."""
    value = (int(time.time() * 1000) << 80) | int.from_bytes(os.urandom(10), "big")
    return "".join(CROCKFORD[(value >> shift) & 0x1F] for shift in range(125, -1, -5))


def parse_args(argv):
    parser = argparse.ArgumentParser(
        prog=PROG,
        description="Drive a program in a pty from a step file.",
        epilog="Arguments after `--` are handed to the child unchanged.",
    )
    parser.add_argument("--script", required=True, help="the step file to run")
    parser.add_argument("--out", help="where the dumps go (default: <scratch>/out)")
    parser.add_argument("--data", help="parent of the scratch tree (default: the temp dir)")
    parser.add_argument("--config", help="the config file to link into scratch")
    parser.add_argument("--bin", help="the program to drive (default: <repo>/target/debug/aigentic)")
    parser.add_argument("--cwd", help="the child's cwd (default: <scratch>/cwd)")
    parser.add_argument("--cols", type=int, default=100, help="pty width (20..400)")
    parser.add_argument("--rows", type=int, default=30, help="pty height (5..200)")
    parser.add_argument("--wait", type=float, default=30.0, help="seconds a wait: step may take")
    parser.add_argument("--exit-time", type=float, default=5.0, help="seconds the child may take to exit")
    parser.add_argument("--keep", action="store_true", help="keep the scratch tree")
    parser.add_argument("child", nargs=argparse.REMAINDER, help="the child's own arguments, after `--`")
    args = parser.parse_args(argv)
    if args.child and args.child[0] == "--":
        args.child = args.child[1:]
    return args


def default_bin():
    """`target/debug/aigentic` of the repository this script lives in."""
    here = os.path.dirname(os.path.realpath(__file__))
    return os.path.join(os.path.dirname(os.path.dirname(here)), "target", "debug", "aigentic")


def default_config():
    """`$XDG_CONFIG_HOME/aigentic/config.toml`, else `~/.config/aigentic/config.toml`."""
    home = os.environ.get("HOME", ".")
    base = os.environ.get("XDG_CONFIG_HOME") or os.path.join(home, ".config")
    return os.path.join(base, "aigentic", "config.toml")


def parse_steps(lines):
    """The step file's commands as (line number, verb, argument).

    A refusal names the line, so a broken script is fixable from one message.
    """
    steps = []
    for number, line in enumerate(lines, 1):
        stripped = line.strip()
        if not stripped or stripped.startswith("#"):
            continue
        if ":" not in stripped:
            refuse("script line %d: expected `<verb>: <argument>`, got %r" % (number, stripped))
        verb, _, argument = stripped.partition(":")
        verb = verb.strip()
        argument = argument.strip()
        if verb not in VERBS:
            refuse("script line %d: unknown verb %r (one of %s)" % (number, verb, ", ".join(VERBS)))
        if not argument:
            refuse("script line %d: %r has no argument" % (number, verb))
        if verb == "key" and argument not in KEYS:
            refuse("script line %d: unknown key %r (one of %s)" % (number, argument, ", ".join(sorted(KEYS))))
        if verb == "dump" and not DUMP_NAME.match(argument):
            refuse("script line %d: dump name %r must be letters, digits, `-` or `_`" % (number, argument))
        if verb == "sleep":
            try:
                argument = float(argument)
            except ValueError:
                refuse("script line %d: sleep takes seconds, got %r" % (number, argument))
        if verb == "wait":
            try:
                re.compile(argument)
            except re.error as exc:
                refuse("script line %d: wait takes a regex (%s), got %r" % (number, exc, argument))
        steps.append((number, verb, argument))
    return steps


def load_pyte(needed):
    """The pyte module, or None: only wait: and dump: steps render a screen."""
    try:
        return importlib.import_module("pyte")
    except ImportError:
        if needed:
            refuse("pyte is needed for wait: and dump: steps: python3 -m pip install --user pyte")
        return None


def pyte_version(pyte):
    if pyte is None:
        return "none"
    try:
        return importlib.metadata.version("pyte")
    except importlib.metadata.PackageNotFoundError:
        return "unknown"


def make_tree(scratch, config, out):
    """Create the scratch tree and link the config into it.

    The config is a symlink, never a copy: the child reads the person's
    profile and model, while `history` and `server.toml` stay in scratch.
    """
    config_home = os.path.join(scratch, "config")
    link = os.path.join(config_home, "aigentic", "config.toml")
    os.makedirs(os.path.dirname(link))
    os.symlink(config, link)
    data_home = os.path.join(scratch, "data")
    os.makedirs(os.path.join(data_home, "aigentic", "threads"))
    dirs = {}
    for name in ("cache", "state", "run", "home", "cwd"):
        dirs[name] = os.path.join(scratch, name)
        os.makedirs(dirs[name])
    dumps = os.path.abspath(out or os.path.join(scratch, "out"))
    os.makedirs(dumps, exist_ok=True)
    return link, config_home, data_home, dirs, dumps


def child_temp(scratch):
    """The child's TMPDIR, and whether it is inside the scratch tree.

    Under the scratch tree when a daemon socket fits there, which keeps the
    child's temp files with everything else the run made. macOS's temp dir is
    too long for that, so there the child gets a directory of its own under
    `/tmp` instead, and the driver removes it with the tree.
    """
    inside = os.path.join(scratch, "tmp")
    if len(inside) + SOCKET_TAIL <= SOCKET_LIMIT:
        os.makedirs(inside)
        return inside, True
    root = os.path.realpath(SHORT_TMP_ROOT)
    if not os.path.isdir(root) or not os.access(root, os.W_OK):
        refuse(
            "no temp dir short enough for the daemon's socket: %s makes a %d-byte "
            "socket path and %s is not writable"
            % (inside, len(inside) + SOCKET_TAIL, root)
        )
    for _ in range(5):
        path = os.path.join(root, "aigentic-pty-" + random_name())
        try:
            os.mkdir(path)
        except FileExistsError:
            continue
        return path, False
    fail("cannot make a temp dir under %s" % root)
    raise SystemExit(1)


def child_env(config_home, data_home, dirs, temp):
    """The environment the child gets: every XDG path and HOME inside scratch."""
    env = dict(os.environ)
    env.update(
        {
            "HOME": dirs["home"],
            "XDG_CONFIG_HOME": config_home,
            "XDG_DATA_HOME": data_home,
            "XDG_CACHE_HOME": dirs["cache"],
            "XDG_STATE_HOME": dirs["state"],
            "XDG_RUNTIME_DIR": dirs["run"],
            "TMPDIR": temp,
        }
    )
    return env


class Session:
    """The child, its pty, every byte it wrote and, with pyte, its screen."""

    def __init__(self, pid, fd, screen, stream):
        self.pid = pid
        self.fd = fd
        self.screen = screen
        self.stream = stream
        self.raw = bytearray()
        self.eof = False
        self.exit_code = None

    def read(self, seconds):
        """Read one chunk if it arrives within `seconds`; False on timeout or EOF."""
        readable, _, _ = select.select([self.fd], [], [], max(0.0, seconds))
        if not readable:
            return False
        try:
            chunk = os.read(self.fd, 65536)
        except OSError as exc:
            if exc.errno != errno.EIO:
                raise
            chunk = b""
        if not chunk:
            self.eof = True
            return False
        self.raw += chunk
        if self.stream is not None:
            self.stream.feed(bytes(chunk))
        return True

    def poll(self):
        """The child's exit code once reaped, else None. A signal is negative."""
        if self.exit_code is None:
            pid, status = os.waitpid(self.pid, os.WNOHANG)
            if pid == self.pid:
                self.exit_code = os.waitstatus_to_exitcode(status)
                self.eof = True
        return self.exit_code

    def text(self):
        if self.screen is None:
            return ""
        return "\n".join(self.screen.display)


def spawn(binary, argv, env, cwd, cols, rows, pyte):
    """Fork the child onto a pty of the given size and return its session."""
    pid, fd = pty.fork()
    if pid == 0:
        try:
            os.chdir(cwd)
            os.execvpe(binary, argv, env)
        except OSError as exc:
            os.write(2, ("%s: cannot run %s: %s\n" % (PROG, binary, exc.strerror)).encode())
        os._exit(127)
    fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", int(rows), int(cols), 0, 0))
    screen = pyte.Screen(cols, rows) if pyte is not None else None
    stream = pyte.ByteStream(screen) if pyte is not None else None
    if screen is not None:
        # The tui asks the terminal where its cursor is and waits for the reply;
        # nothing on a bare pty answers, so the emulator does.
        screen.write_process_input = lambda data: answer(fd, data)
    return Session(pid, fd, screen, stream)


def answer(fd, data):
    """Send a terminal's reply, such as a cursor position, back to the child."""
    try:
        os.write(fd, data.encode("utf-8"))
    except OSError as exc:
        if exc.errno != errno.EIO:
            raise


def rendered(session):
    """The screen right-trimmed per line, trailing blank lines dropped."""
    lines = [line.rstrip() for line in session.text().split("\n")]
    while lines and not lines[-1]:
        lines.pop()
    if not lines:
        return ""
    return "\n".join(lines) + "\n"


def write_dump(dumps, name, session, echo):
    """Write `<name>.txt` and `<name>.raw`, and print the screen under a marker."""
    text = rendered(session)
    with open(os.path.join(dumps, name + ".txt"), "wb") as handle:
        handle.write(text.encode("utf-8", "replace"))
    with open(os.path.join(dumps, name + ".raw"), "wb") as handle:
        handle.write(bytes(session.raw))
    if echo:
        sys.stdout.write("==== %s ====\n%s" % (name, text))
        sys.stdout.flush()


def wait_for(session, pattern, limit):
    """Run until the screen matches; ('ok', None), ('exited', code) or ('timed out', None)."""
    deadline = time.monotonic() + limit
    while True:
        if re.search(pattern, session.text()):
            return "ok", None
        code = session.poll()
        if code is not None and session.eof:
            return "exited", code
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            return "timed out", None
        session.read(min(0.02, remaining))


def pump(session, seconds):
    """Read the pty for `seconds`, so a sleep: step lets the child work."""
    end = time.monotonic() + seconds
    while True:
        remaining = end - time.monotonic()
        if remaining <= 0:
            return
        session.read(min(0.02, remaining))


def describe(code):
    if code < 0:
        return "killed by signal %d" % -code
    return "exited %d" % code


def type_keys(session, data, verb):
    """Write to the pty, or say so when the child's end is already gone."""
    try:
        os.write(session.fd, data)
    except OSError as exc:
        if exc.errno != errno.EIO:
            raise
        raise Failure("%s failed: the child's pty is closed" % verb)


def run_steps(session, steps, wait, dumps):
    for number, verb, argument in steps:
        if verb == "send":
            type_keys(session, argument.encode("utf-8") + b"\r", "send")
        elif verb == "key":
            type_keys(session, KEYS[argument], "key")
        elif verb == "wait":
            state, code = wait_for(session, argument, wait)
            if state == "exited":
                raise Failure("child %s while waiting for %s" % (describe(code), argument))
            if state == "timed out":
                raise Failure("wait timed out: %s" % argument)
        elif verb == "sleep":
            pump(session, argument)
        elif verb == "dump":
            write_dump(dumps, argument, session, True)


def await_exit(session, limit):
    """('exited', code) if the child leaves within `limit`, else terminate it: ('stalled', None)."""
    end = time.monotonic() + limit
    while time.monotonic() < end:
        code = session.poll()
        if code is not None:
            return "exited", code
        session.read(0.02)
    for number, grace in ((signal.SIGTERM, 1.0), (signal.SIGKILL, 5.0)):
        try:
            os.kill(session.pid, number)
        except ProcessLookupError:
            pass
        end = time.monotonic() + grace
        while time.monotonic() < end:
            if session.poll() is not None:
                return "stalled", None
            session.read(0.02)
    return "stalled", None


def main(argv):
    args = parse_args(argv)

    temp = os.path.realpath(tempfile.gettempdir())
    data = os.path.realpath(args.data or tempfile.gettempdir())
    if data != temp and not under(data, temp):
        refuse("--data must be the temp dir or under it, not %s" % data)

    scratch = os.path.join(data, "aigentic-pty-" + ulid())
    cwd = os.path.realpath(args.cwd or os.path.join(scratch, "cwd"))
    if not under(cwd, data):
        refuse("--cwd must be under %s, not %s" % (data, cwd))

    config = os.path.abspath(args.config or default_config())
    if not os.path.isfile(config):
        refuse("--config not found: %s" % config)
    try:
        with open(config, "r", encoding="utf-8", errors="replace") as handle:
            lines = handle.read().splitlines()
    except OSError as exc:
        refuse("--config is unreadable: %s: %s" % (config, exc.strerror))
    for number, line in enumerate(lines, 1):
        if THREADS_DIR.match(line):
            refuse(
                "--config sets threads_dir on line %d, which would send this check's threads "
                "out of scratch: %s" % (number, config)
            )

    binary = os.path.abspath(args.bin or default_bin())
    if not (os.path.isfile(binary) and os.access(binary, os.X_OK)):
        refuse("no debug binary at %s: cargo build -p aigentic-tui" % binary)

    try:
        with open(args.script, "r", encoding="utf-8", errors="replace") as handle:
            script = handle.read().splitlines()
    except OSError as exc:
        refuse("--script is unreadable: %s: %s" % (args.script, exc.strerror))
    steps = parse_steps(script)

    if not 20 <= args.cols <= 400:
        refuse("--cols must be 20..400, not %d" % args.cols)
    if not 5 <= args.rows <= 200:
        refuse("--rows must be 5..200, not %d" % args.rows)
    pyte = load_pyte(any(verb in ("wait", "dump") for _, verb, _ in steps))

    link, config_home, data_home, dirs, dumps = make_tree(scratch, config, args.out)
    os.makedirs(cwd, exist_ok=True)
    temp = None
    # Everything created below is removed on the way out, a failure included.
    try:
        temp, temp_inside = child_temp(scratch)

        argv_child = [binary]
        # A shell exits on an unknown long option, so only aigentic gets the
        # flag; its config is pinned by XDG_CONFIG_HOME either way.
        if os.path.basename(binary) == "aigentic":
            argv_child += ["--config", link]
        argv_child += args.child

        for key, value in (
            ("bin", binary),
            ("cwd", cwd),
            ("config", config),
            ("data", data),
            ("scratch", scratch),
            ("tmp", temp),
            ("out", dumps),
            ("screen", "%dx%d" % (args.cols, args.rows)),
            ("pyte", pyte_version(pyte)),
        ):
            sys.stdout.write("%s %s=%s\n" % (PROG, key, value))
        sys.stdout.flush()

        session = spawn(
            binary,
            argv_child,
            child_env(config_home, data_home, dirs, temp),
            cwd,
            args.cols,
            args.rows,
            pyte,
        )
        try:
            return run(session, steps, args, dumps)
        finally:
            os.close(session.fd)
    finally:
        if not args.keep:
            shutil.rmtree(scratch, ignore_errors=True)
            if temp is not None and not temp_inside:
                shutil.rmtree(temp, ignore_errors=True)


def run(session, steps, args, dumps):
    """Drive the child to the end of the steps and report how it went."""
    try:
        run_steps(session, steps, args.wait, dumps)
    except Failure as failure:
        write_dump(dumps, "final", session, False)
        fail(str(failure))
        return 1
    state, code = await_exit(session, args.exit_time)
    write_dump(dumps, "final", session, False)
    if state == "stalled":
        fail("child did not exit")
        return 1
    if code != 0:
        fail("child %s" % describe(code))
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
