# aigentic-tools

Built-in tools for the Aigentic harness and the registry that holds them.
Each tool implements the `Tool` trait from `aigentic-core` with a
schemars-derived argument schema and a risk class.

| Tool | Class | Does |
| --- | --- | --- |
| `read_file` | read | Read a UTF-8 file, capped head and tail |
| `list_dir` | read | One directory, sorted, `name/` for directories, `name  <bytes>` for files |
| `grep` | read | Regex search, recursive, `.gitignore` and hidden files respected, binaries skipped, `path:line:text`, capped at 200 matches then by bytes |
| `write_file` | write | Create or replace a file |
| `edit_file` | write | Replace one exact occurrence of a string; refuses on zero or many |
| `bash` | exec | Run a command with a timeout and output cap |

## Registry

`ToolRegistry::builtin(workdir)` holds the six; `register` adds more and
refuses a duplicate name; `specs()` is sorted by name so the request the
model sees is byte-stable; `names()` is what a skill's `requires` is
checked against. MCP-backed tools join the registry in phase 3 step 6.

## Shared working directory

All tools take a `Workdir`, a cheap-to-clone handle to the current
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
