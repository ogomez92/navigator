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

| Key                | Action                                  |
|--------------------|-----------------------------------------|
| letter             | Jump to next entry starting with letter |
| Shift + Up/Down    | Extend contiguous selection             |
| Ctrl + Space       | Toggle selection of focused entry       |
| Ctrl + A           | Select all                              |
| Enter              | Open folder / launch file               |
| Backspace          | Parent folder                           |
| F5                 | Refresh                                 |
| Ctrl + C / X / V   | Copy / cut / paste (via rclone)         |
| Del                | Delete (via rclone purge)               |
| F2                 | Rename                                  |
| Alt + Up           | Parent folder                           |
