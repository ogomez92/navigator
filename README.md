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
