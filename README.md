# navigator

Scalable, accessible Windows file explorer in Rust.

> **⚠️ Heads up — this is custom, personal software. It is NOT a drop-in Explorer replacement.**
>
> navigator deliberately does several things differently from Windows Explorer. If
> you run it on your real files, know these up front:
>
> - **Delete does NOT use the Windows Recycle Bin.** `Del` moves files to a hidden
>   `.trash` folder at the root of the *same drive* (`C:\.trash\`, `D:\.trash\`, …).
>   These files will **not** appear in Explorer's Recycle Bin and Windows "Restore"
>   won't find them. The trash is **never emptied automatically** — empty it yourself
>   to reclaim space. `Ctrl+Z` can undo a delete, but only while navigator is still
>   running (see below).
> - **Copy / cut use navigator's own clipboard, not Windows'.** Copy/cut/paste go
>   through `clipboard.json` next to the exe, so they work between two navigator
>   windows but do **not** interact with Explorer's clipboard. (The one exception is
>   `Ctrl+Shift+C` "copy path", which writes plain text to the real Windows clipboard.)
> - **Undo is in-memory only.** Closing navigator discards the undo history. Files
>   already staged on disk stay there, but you can no longer `Ctrl+Z` them back.
> - **File operations require `rclone` on your `PATH`** (and `7z` for `Ctrl+E`
>   extraction). Every copy / move / delete / rename shells out to `rclone` — if it
>   isn't installed, file operations silently do nothing useful.
> - **Settings & state live next to the .exe, not in `%APPDATA%`.** `config.toml`,
>   `clipboard.json`, `clipboard_history.json`, `plugins/`, and the remote download
>   cache all sit in the exe's folder. Don't drop the exe somewhere read-only (e.g.
>   `Program Files`) or it can't save. If config ever misbehaves after an update,
>   delete `config.toml` and relaunch — defaults regenerate (there is no migration).
> - **Remote files download to a local cache that is never cleaned up.** Opening a
>   file on an rclone remote stages a copy under `.remote-cache/` and leaves it there.
>   Delete that folder manually to free space.
>
> In short: it's a single-user tool tuned to one person's setup. Try it on throwaway
> data first.

## Goals

- **Accessible first.** Native Win32 controls (`SysListView32`, standard edits, toolbars) — MSAA/UIA work without extra plumbing. Screen readers see the app as a regular Explorer-class window.
- **Screen-reader output** via the [Prism](https://github.com/prismatoid/prism) C library (prebuilt and vendored in `crates/navigator-prism/vendor/`). Used for supplementary announcements (status, progress, warnings) on top of native a11y.
- **File operations via `rclone`.** No `SHFileOperation`. Copy/cut/paste spawn `rclone copyto` / `moveto` with `--use-json-log` so we can parse errors and detect up-front with `--dry-run` exactly what a paste would destroy.
- **Paste asks a merge question, not a per-file one.** There's no "this file exists — replace it?" dialog for every collision. A paste runs in a mode (`Add new only`, `Update`, `Replace`, or `Mirror`) and only stops to confirm when a dry-run proves something at the destination would actually be lost — so pasting into a populated folder is usually silent. The default, `Update`, copies what differs and never replaces a destination file that's *newer* than the source. `Ctrl+Shift+V` picks the mode per paste and is the only route to `Mirror`, which makes the destination match the source exactly and deletes what the source doesn't have. Set the default under Options → Rclone.
- **Progress you can listen to, without a file-by-file monologue.** A long copy or delete announces where it is every few seconds — "45 percent, 90 of 200" — and nothing at all if it finishes faster than that. It counts the whole operation, not whichever `rclone` process happens to be running, so the number only ever climbs. Change the cadence (or switch it off with `0`) under Options → Speech. Options → Rclone can also open a progress window with the current file, transfer rate, ETA and a working Cancel button.
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

Requires Rust 1.95+ and the MSVC toolchain (`x86_64-pc-windows-msvc`). The
prebuilt [Prism](https://github.com/prismatoid/prism) library is **vendored in
the repo** (`crates/navigator-prism/vendor/`), so a fresh clone builds with no
external setup:

    cargo build --release

Run:

    cargo run --release

> Only the *dynamic* prism build is vendored. To build with `--features static`,
> set the `PRISM_DIR` environment variable to a full prism distribution that
> includes the `static` tree.

> **Runtime requirements:** `rclone` (for all file operations) and `7z` (for
> `Ctrl+E` extraction) must be on your `PATH`. `prism.dll` is copied next to the
> built exe automatically.

Personal install (builds release and copies the exe to a bin dir as `x.exe`;
destination defaults to `~/stuff/bin/x.exe`, override with `NAVIGATOR_INSTALL`):

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
| Ctrl + Shift + V             | Paste special — choose how existing items are handled |
| Ctrl + Alt + C / X           | Append to copy / cut clipboard                      |
| Ctrl + Shift + C             | Copy full path(s) to OS clipboard                   |
| Alt + C / X                  | Copy / cut selection to OS clipboard (CF_HDROP)     |
| Ctrl + Alt + V               | Paste from OS clipboard via Windows shell (no rclone) |
| Ctrl + Z                     | Undo last clipboard / paste / delete                |
| Del                          | Delete to per-volume `.trash`                       |
| Ctrl + N                     | New folder (prompts for name)                       |
| Ctrl + Shift + N             | New file (name needs a `.type`) + open in default app |
| F2                           | Rename                                              |
| Ctrl + F                     | Find in folder                                      |
| Ctrl + H / Ctrl + Shift + H  | Toggle hidden / system files                        |
| Ctrl + 1..9, Ctrl + 0        | Jump to hotspot slot 1..10                          |
| Ctrl+Shift+1..9, Ctrl+Shift+0| Save selection to hotspot slot 1..10                |
