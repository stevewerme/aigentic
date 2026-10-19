# tools/pty — drive a tui in a pty

`drive.py` starts a program on a pseudo-terminal, types a step file at it, and
writes what the screen shows. A step that changes what `crates/tui` draws runs
it against a debug build, so the check happens inside the loop instead of by
hand afterwards.

## Setup

```bash
python3 -m pip install --user pyte
```

Nothing else: the driver is one file and no venv or requirements file is
committed. Without `pyte`, a step file with `wait:` or `dump:` is refused.

## The step file

One verb per line; blank lines and `#` comments are ignored.

```
send: /new                   # write the text, then CR
wait: front                  # read until the rendered screen matches
key: C-c                     # one of enter, tab, esc, up, down, C-c, C-d
sleep: 2                     # read for that long
dump: front                  # write front.txt and front.raw, print the screen
```

`wait:` times out after `--wait` seconds with `wait timed out: <regex>`; a child
that exits while one is pending fails with `child exited <n> while waiting for
<regex>`. Both write `final.txt` and `final.raw` first.

## The command

```bash
cargo build -p aigentic-tui
python3 tools/pty/drive.py --script /tmp/<n>-pty.steps --out /tmp/<n>-pty
```

```
drive.py --script FILE [--out DIR] [--data DIR] [--config FILE] [--bin FILE]
         [--cwd DIR] [--cols N] [--rows N] [--wait S] [--exit-time S] [--keep]
         [-- ARGS…]
```

The defaults are the debug build (`<repo>/target/debug/aigentic`), your config
(`$XDG_CONFIG_HOME/aigentic/config.toml`), the temp dir as `--data`, a 100×30
screen, 30 s per `wait:` and 5 s for the child to exit. Arguments after `--` go
to the child.

It prints one `drive.py <key>=<value>` line per fact before the child starts —
`bin`, `cwd`, `config`, `data`, `scratch`, `tmp`, `out`, `screen`, `pyte` — so a
dump says which binary and which screen produced it.

Exit `0`: every step ran and the child exited 0. Exit `1`: a step failed, or the
child exited non-zero, or it was still alive after `--exit-time` and was
terminated. Exit `2`: a refusal, one line on stderr.

## What the scratch tree gives

The run happens in a fresh `<data>/aigentic-pty-<ulid>/`, deleted on exit unless
`--keep`. Inside it: `config/aigentic/config.toml` (a symlink to your config
file), `data/aigentic/threads`, `cache`, `state`, `run`, `home`, `cwd` and `out`.
The child's `HOME`, `XDG_CONFIG_HOME`, `XDG_DATA_HOME`, `XDG_CACHE_HOME`,
`XDG_STATE_HOME` and `XDG_RUNTIME_DIR` point inside it, so the threads, the
history file, `server.toml` and any `~` path a config expands are scratch. The
API key is inherited from your environment.

`TMPDIR` is the one path that can be elsewhere. The tui's in-process daemon puts
its socket at `<TMPDIR>/aigentic-<ulid>/aigentic.sock`, a unix socket path is
capped near 103 bytes, and macOS's temp dir is long enough that a scratch tree
under it makes a 150-byte path. So the child gets `<scratch>/tmp` when a socket
fits there, and otherwise a directory of its own under `/tmp` (the `tmp=` line
says which); either is made and deleted by the driver, and `--keep` keeps it.

Three things it never touches:

- **never a real project directory** — `--cwd` must be under `--data`; a cwd in
  a repository would load that repository's `.env` and register it as the live
  project;
- **never a `.env`** — neither the scratch cwd nor any ancestor of it holds one,
  and the driver reads no `.env` and copies none;
- **never the installed binary** — the default is this repository's debug build.

Because the config is a link to the file and not its directory, `instructions.md`,
`server.toml` and any workspaces file beside your real config are not seen: the
child starts with no global instructions and no workspaces. A config that sets
`threads_dir`, and a `--data` outside the temp dir, are refused (`--config`,
`--data must be`, `--cwd must be`).

## Afterwards

Paste each dump's text on the issue, and the `.raw` file's bytes when rows appear
or vanish (pyte mis-renders scroll-region inserts). Then delete the dumps:
`rm -rf /tmp/<n>-pty`. A check that runs past a minute is a finding, not a
stall.
