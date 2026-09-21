# aigentic-tools

Built-in tools for the Aigentic harness: `read_file`, `write_file` and `bash`.
Each implements the `Tool` trait from `aigentic-core` with a schemars-derived
argument schema and a risk class (`Read`, `Write`, `Exec`).

## Shared working directory

All three tools take a `Workdir`, a cheap-to-clone handle to the current
directory. A `cd` in one bash call is seen by the next bash call and by
relative paths in the file tools. The shell reports its final directory by
writing a temp file and renaming it over the real one, so a partial path is
never read. A command that `exit`s early or times out leaves the directory
unchanged.

## Output capping

Every tool caps what it returns (32 KiB per stream by default). Long output
keeps its head and its tail with a line in between stating how many bytes
were omitted. Bash captures into a bounded buffer, so memory stays at
`2 * cap` per stream however much a command prints.

## Bash and process groups

The shell runs in its own process group. When the call ends, the whole group
is torn down: SIGTERM, up to two seconds for the shell to exit and the pipes
to close, then SIGKILL. This happens on normal exit as well as on timeout.

**Rule: background processes never outlive a bash call.** `sleep 60 &`
returns as soon as the shell exits, and the sleep is killed. Without this,
a backgrounded process holding the stdout pipe open would block the call
until the timeout. A `detach` mechanism for processes that should survive
(a dev server, a watcher) is a phase 3 addition alongside policy.

The order on normal exit is: wait for the shell to exit, kill the group,
then drain stdout and stderr to EOF. On timeout the shell is still running
when the group is killed; the result says so, includes whatever output was
captured, and sets `is_error`.

Process-group handling is Unix only. On other platforms a clearly marked
stub kills the shell alone, and anything it started may survive.
