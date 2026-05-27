# navigator

Scalable, accessible Windows file explorer in Rust.

## Goals

- **Accessible first.** Native Win32 controls (`SysListView32`, standard edits, toolbars) — MSAA/UIA work without extra plumbing. Screen readers see the app as a regular Explorer-class window.
- **Screen-reader output** via the [Prism](https://github.com/prismatoid/prism) C library (local at `D:\code\libs\prism`). Used for supplementary announcements (status, progress, warnings) on top of native a11y.
- **File operations via `rclone`.** No `SHFileOperation`. Copy/cut/paste spawn `rclone copyto` / `moveto` with `--use-json-log` so we can parse errors and detect overwrites up-front with `--dry-run`.
- **Extensible** through Rust plugins loaded as DLLs via a stable C ABI (`navigator-plugin-api`).
- **Fast.** Directory listing via raw `FindFirstFileW`. Virtual `ListView` (LVS_OWNERDATA) so million-entry folders render instantly.

## Layout

    crates/
      navigator-core         shared types (paths, entries, events)
      navigator-plugin-api   stable C ABI for plugins
      navigator-prism        FFI wrapper for prism TTS/screen-reader
      navigator-rclone       rclone driver, JSON log parser
      navigator-fs           Win32 dir scan, path utilities
      navigator-gui          Win32 window + ListView shell
      navigator              binary entry point
    plugins/
      sample                 example plugin

## Build

Requires Rust 1.95+, MSVC toolchain, and `prism-windows-x64` at `D:\code\libs\prism`.

    cargo build --release

Run:

    cargo run --release

Personal install:

    .\r.cmd      # PowerShell / cmd
    ./r.sh       # Git Bash / WSL

## CLI

    navigator [OPTIONS] [PATH]

| Arg / flag              | Effect                                                           |
|-------------------------|------------------------------------------------------------------|
| `<PATH>`                | Local path (`C:\foo`, `.`) or rclone remote (`mac:downloads`)    |
| `-r`, `--remote <SPEC>` | Open an rclone remote. `SPEC` is `name` or `name:sub/path`       |
| `-h`, `--help`          | Print usage                                                      |

Examples:

    navigator
    navigator .
    navigator C:\Users\me\Downloads
    navigator -r mac:Downloads/incoming
    navigator -r gdrive                    # bare name = remote root
    navigator mac:Downloads/incoming       # same as -r, no flag needed

## Remotes (rclone)

navigator browses any rclone remote as if it were a local folder. Everything
you configured in `%APPDATA%\rclone\rclone.conf` (S3, Google Drive, SFTP,
WebDAV, etc.) is available from the **Tools → Connect to remote…** menu,
which drops you into a virtual "Remotes" view listing each remote. Activating
one opens its root; copy/cut/paste/delete/rename all flow through the
existing rclone pipeline, so remote↔local and remote↔remote transfers work
the same way local-only ops do.

Remote paths display in rclone form (`remote:sub/path`) in the address bar.
Typing one directly there — or passing it on the command line — jumps
straight to that location without going through the menu.

### Opening remote files

Pressing Enter on a file inside a remote triggers a download-and-edit
flow:

1. The file is fetched into `<exe_dir>/.remote-cache/<remote>/<sub/path>/`.
2. Once download finishes, the staged copy is handed to `ShellExecute` so
   the OS opens it in whatever app the extension is associated with.
3. navigator keeps a `notify` watcher on the cache directory. When you
   save through your editor (mtime bumps), it pops a Yes/No prompt —
   "Upload changes back to `remote:path`?". Yes spawns an rclone upload;
   No leaves the staged copy alone.
4. Staged files are never auto-purged. Same stance as `.trash/`: if you
   want to free disk, delete `.remote-cache/` manually.

## Key bindings

| Key                          | Action                                              |
|------------------------------|-----------------------------------------------------|
| letter                       | Type-ahead jump (>1s gap resets buffer)             |
| Shift + Up/Down              | Extend contiguous selection                         |
| Ctrl + Space                 | Toggle selection of focused entry                   |
| Ctrl + A                     | Select all                                          |
| Enter                        | Open folder / launch file                           |
| Backspace / Alt + Up         | Parent folder                                       |
| Alt + Left / Right           | History back / forward                              |
| F5                           | Refresh                                             |
| Ctrl + C / X / V             | Copy / cut / paste (via rclone)                     |
| Ctrl + Alt + C / X           | Append to copy / cut clipboard                      |
| Ctrl + Shift + C             | Copy full path(s) to OS clipboard                   |
| Ctrl + Z                     | Undo last clipboard / paste / delete                |
| Del                          | Delete to per-volume `.trash`                       |
| F2                           | Rename                                              |
| Ctrl + F                     | Find in folder                                      |
| Ctrl + H / Ctrl + Shift + H  | Toggle hidden / system files                        |
| Ctrl + 1..9, Ctrl + 0        | Jump to hotspot slot 1..10                          |
| Ctrl+Shift+1..9, Ctrl+Shift+0| Save selection to hotspot slot 1..10                |
