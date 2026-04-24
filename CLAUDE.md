# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project

Accessible Windows file explorer, written in Rust. Windows-only (`x86_64-pc-windows-msvc`), Rust 1.95+, edition 2024, workspace resolver v3.

## Build & run

```
cargo build --release
cargo run --release [initial_path]
cargo test -p <crate>                 # run tests for a single crate
cargo test -p navigator-rclone log    # single test / module
cargo clippy --workspace --all-targets
cargo fmt --all
```

Runtime env: `NAVIGATOR_LOG` sets the `tracing` `EnvFilter` (default `info`).

### Native dependencies

- **Prism** (screen-reader / TTS C library) is linked by `crates/navigator-prism/build.rs`.
  - Resolution: `PRISM_DIR` env var, else `D:\code\libs\prism\prism-windows-x64`.
  - Expects `<base>/dynamic/<profile>/lib/prism.lib` + `<base>/dynamic/<profile>/bin/prism.dll`. Build copies `prism.dll` next to the output binary. `--features static` switches to `static/<profile>/lib`.
  - `<profile>` is `debug` for dev builds, `release` otherwise.
- **rclone** must be on `PATH` at runtime — all file operations shell out to it.

## Architecture

Thin binary, fat workspace. `crates/navigator/src/main.rs` only parses args + initializes tracing, then calls `navigator_gui::run`. Everything else lives in sibling crates.

### Crates (dependency direction: top → bottom)

- **`navigator`** — binary entry point.
- **`navigator-gui`** — Win32 window shell. The only crate that knows about HWNDs. Owns `AppState`, the message loop, the virtual `SysListView32`, plugin host wiring, speech sink, file watcher, and worker threads.
- **`navigator-config`** — TOML config at `<exe_dir>/config.toml` (never `%APPDATA%`). `ConfigHandle` is an `Arc<RwLock<Config>>` clone-able handle. Also defines shortcut actions.
- **`navigator-plugin-api`** — stable C ABI for plugins. Plugins are `cdylib` crates exporting `navigator_plugin_entry`. Strings crossing the boundary are `*const u8 + len` (UTF-8), everything `#[repr(C)]`. Loaded with `libloading`.
- **`navigator-prism`** — safe FFI wrapper around the prism C library. `Prism` is a process-wide singleton guarded by an `AtomicBool`; `Speaker` handles are `Send` but not `Sync`.
- **`navigator-rclone`** — rclone driver. Spawns `rclone` with `--use-json-log --stats=1s`, parses each stdout line as a structured log record. Emits `OpEvent::{Progress, Log, Done}` on a crossbeam channel. Pre-flight `--dry-run` detects overwrites before the real op starts.
- **`navigator-fs`** — directory scanning via raw `FindFirstFileExW` with `FindExInfoBasic` + `FIND_FIRST_EX_LARGE_FETCH`. Exposes `read_dir`, `list_drives` (for the virtual "This PC" view), and `search_recursive`.
- **`navigator-core`** — shared value types (`NavPath`, `Entry`, `Selection`, `Event`, `Error`). No GUI / OS code; safe to use from plugins.
- **`plugins/sample`** — example plugin.

### Threading model

One UI thread (the Win32 message loop) and several workers. All cross-thread comms go through `crossbeam-channel` or Win32 `PostMessageW`.

- **`navigator-scan`** — long-lived worker. Handles `ScanCmd::List` (directory scan) and `ScanCmd::Search` (recursive search). Posts results back as `WMAPP_DIR_LISTED` / `WMAPP_SEARCH_RESULTS`.
- **`navigator-plugin-nav`** — bridges plugin nav requests into `AppState::navigate` via a weak `Arc` so it dies with the app.
- **`navigator-rclone-op` / `navigator-batch-op` / `navigator-batch-delete`** — short-lived per-operation threads. They hold a `WorkerCtx` (cheap clone of rclone driver, speech sender, scan sender, optional progress handle) — never borrow `AppState`.
- **Speech sink** — `SpeechSink` owns its own thread; everything (plugins, workers, UI) just sends `Utterance` messages.
- **File watcher** — `notify::RecommendedWatcher` in `AppState.watcher`. Replaced on each navigation; dropping unsubscribes.

### Virtual ListView

The main control is `SysListView32` in `LVS_OWNERDATA` (virtual) mode. The backing store is `Model` in `navigator-gui/src/model.rs`. Row data is pulled on-demand via `LVN_GETDISPINFO`, so million-entry folders render instantly. Mutations don't rebuild the control — they update `Model` and send targeted `LVM_REDRAWITEMS` (`WMAPP_REDRAW_ROW`).

**Column visibility is dynamic.** `Name` always shows at iSubItem 0; `Size`/`Type`/`Modified` are toggled in `config.general.columns`. `ListView::create` walks `listview::visible_columns(&cols)` to insert only enabled columns, so the physical iSubItem indices are a *prefix* of the logical enum. `fill_dispinfo` must go through `listview::column_for_subitem(&cols, sub)` to recover the `LogicalColumn`; indexing with the old hard-coded `COL_NAME`/`COL_SIZE`/… constants is wrong once a column is hidden. Options → Columns commits via `AppState::reconfigure_listview_columns`, which posts `WMAPP_RECONFIGURE_COLUMNS` so the UI thread tears down + re-inserts columns and refreshes the virtual count. Sort keys (`SortMode::Type` included) are independent of column visibility: you can sort by Type with the Type column hidden.

### Input handling (landmines)

The Win32 input pipeline has two stages in the message pump and several interacting sources of truth. Getting these wrong silently breaks shortcuts.

- **Pump order is accelerators → IsDialogMessageW → TranslateMessage/DispatchMessage** (Petzold order). Reversing the first two lets `IsDialogMessageW` swallow `Ctrl+letter` / `Alt+letter` chords before the accel table sees them — every user-configured shortcut and every static `Ctrl+C/X/V/A/H/F` binding dies silently. Keep accel first.
- **Accel table deliberately omits VK_BACK, VK_DELETE, VK_RETURN.** Those are scoped to the listview via `SetWindowSubclass` in `crates/navigator-gui/src/listview.rs`, which posts `WM_COMMAND(Commands::{Back,Delete,OpenFocused})` to the parent only when the listview actually has focus. If those keys were in the global accel they would fire from inside the address-bar edit too, breaking editing (Backspace navigates up, Delete deletes the selection, Enter fires IDOK).
- **Enter routing.** `IsDialogMessageW` turns `VK_RETURN` into `WM_COMMAND(IDOK)` when no default button exists. The IDOK arm in `handle_command` routes by `GetFocus()` — listview → `open_focused`, address → `navigate_from_address`. Don't blindly call `navigate_from_address` on IDOK; that was the previous bug.
- **Model is source of truth for selection/focus.** `LVN_ITEMCHANGED` must be mirrored into `Model.selection` via `mirror_item_change` (diffs `uOldState` / `uNewState`). Without the mirror, `selected_paths()` / `focus()` are empty, so `op_copy` / `op_delete` / `open_focused` / `run_action` all no-op. `Selection::insert` / `remove` are idempotent for exactly this path; `toggle` flips and is wrong here.
- **Range multi-select needs a second notification.** Virtual (`LVS_OWNERDATA`) listviews fire `LVN_ODSTATECHANGED` (one `NMLVODSTATECHANGE` with `iFrom..=iTo`) for shift-click / shift-arrow / Ctrl+A, *not* per-row `LVN_ITEMCHANGED`. `mirror_range_change` handles it. Without that arm, multi-select rows never make it into `Model.selection` and every batch op says "nothing selected".
- **Refocus after navigate-up.** `AppState.pending_focus: Mutex<Option<NavPath>>` stores the child path before `navigate_up` fires. The post-listing hook (`refocus_after_up` in `window.rs`) consumes it and calls `select_row`. For drive-root → This PC it matches via `navigator_fs::drive_path_from_display` inverse; for regular folders it matches by filename.
- **WM_SETFOCUS on the main hwnd redirects to the listview.** Without it Windows parks focus on the first tabstop (address bar) after first-show and alt-tab-back. Listview is what the user wants 99% of the time.
- **UNC bare shares.** Rust's `Path::is_absolute` returns `false` for `\\host\share` (prefix without root component). `NavPath::new` retries with a trailing `\` so IP-based shares like `\\100.86.173.34\media` navigate.
- **Default shortcuts are populated on first run only.** `default_actions()` seeds Copy / Cut / Paste / F2 / F5 / Hotspots etc. when no `config.toml` exists. There is intentionally **no migration chain** — this is a single-user tool, and if a stale config ever drifts from current defaults we delete `config.toml` and regenerate. Don't add a migration helper; just ship a breaking default change and expect the user to re-run.
- **Listview needs `LVS_EDITLABELS` for F2.** `LVM_EDITLABELW` is a silent no-op without that style. `LVN_BEGINLABELEDITW` / `LVN_ENDLABELEDITW` route the result to `op_rename`.
- **We drive type-ahead, not `SysListView32`.** The control's private prefix buffer is not clearable externally — a letter typed after navigating into a new folder would resume the old buffer. `AppState.type_ahead: Mutex<(String, Instant)>` is our own buffer; the listview subclass consumes `WM_CHAR` (returns `LRESULT(0)`) so the control never accumulates, and `AppState::reset_type_ahead()` runs at the top of every `navigate`. `type_ahead_step(ch)` appends + searches via `model.find_prefix`, auto-resetting after a >1s gap for Explorer-cadence.

### File operations invariant

All mutations (copy, move, delete, rename) go through `navigator-rclone`. No `SHFileOperation`, no direct `DeleteFileW`. Overwrite decisions come from pre-flight (`--dry-run`) plus the `preflight` module's per-item prompt — never from `--ignore-existing` by default.

The preflight TaskDialog offers three choices plus Cancel: `Overwrite`, `Skip`, and `Keep both (append number)`. `Keep both` maps to `ItemChoice::Rename` and delegates to `preflight::unique_numbered_path` to pick a fresh sibling like `foo (1).txt` (Explorer parity — multi-extension files become `archive.tar (1).gz`). For copy paths the batch worker uses `Operation::CopyTo { src, dst }`; for cut paths it reuses `Operation::Rename { src, dst }` with the new dst. `CopyTo` is distinct from `Copy { dest_dir, .. }` because `Copy` always keeps the source filename — don't shove a renamed destination through it.

### Clipboard + undo + trash

- **Clipboard is file-backed**, not the Windows clipboard. `<exe_dir>/clipboard.json` holds `{sources, cut, ts}`; written by copy/cut/append, read by paste. Two running navigator instances share it automatically. The OS clipboard is untouched except by `op_copy_paths` (CF_UNICODETEXT on purpose).
- **Operation history** lives in `<exe_dir>/clipboard_history.json`, capped at `MAX_HISTORY` (20) rolling entries. Feeds File → Recent operations; `WM_INITMENUPOPUP` rebuilds the submenu each time from disk so peer instances' writes show up. Command IDs `Commands::RecentOpsBase..+20` route clicks to `op_restore_from_history`.
- **Undo stack is in-memory only** (`AppState.undo_stack: Mutex<Vec<UndoAction>>`, capped at 50). Variants: `ClipChange { prev }` (reverse copy/cut/append/restore) and `Paste { created, originals, cut_mode }` (copy-undo deletes `created`, cut-undo moves each back to its `originals[i]`) and `Delete { pairs: Vec<(trash, original)> }`. Push happens *before* spawning the worker so Ctrl+Z can reach even in-flight operations.
- **Delete → trash, not purge.** `op_delete` renames each target to `<volume_root>/.trash/<unix_ts>_<counter>/<basename>` on the *same* drive (derived via `volume_root_of`) so the move is atomic — no cross-drive copy. The worker is `run_trash_batch`; undo is `run_revert_delete`, which skips targets whose original path is now re-occupied rather than clobbering. On successful undo the worker arms `AppState::set_pending_focus` with the first restored path, so the subsequent refresh lands the caret back on the recovered row — the worker reaches into `AppState` via the `Weak<AppState>` stored on `WorkerCtx.state` (set up in `AppState::new` via `self_weak: OnceCell<Weak<Self>>` so methods on `&self` can still hand workers a route back). Trash is never auto-purged; closing the app orphans the undo handle but the staged files remain.
- **Clipboard path validity is checked at paste/restore, not at copy.** `op_paste` filters sources via `NavPath::new`; `op_restore_from_history` partitions by `Path::exists()` and announces missing-count without touching the clip file if *all* paths are gone.

### Real Win32 dialogs

`crate::dialog::run_modal(parent, title, cx_dlu, cy_dlu, dlg_proc, init_param)` is the one way to open a modal dialog. It constructs an in-memory `DLGTEMPLATE` (`WS_POPUP | WS_CAPTION | WS_SYSMENU | DS_MODALFRAME | DS_SETFONT | DS_CENTER | DS_3DLOOK`, `WS_EX_CONTROLPARENT`), calls `DialogBoxIndirectParamW`, and the OS registers the default `#32770` dialog class — so MSAA/UIA announce `ROLE_SYSTEM_DIALOG`, Enter/Esc route through `DefDlgProc` to IDOK/IDCANCEL, and Tab traversal works without a custom pump. Controls are still built in `WM_INITDIALOG`. Do **not** roll custom `RegisterClassEx` + message pumps for dialogs — screen readers only see them as generic windows.

- Init state is passed via `Box::into_raw(Box::new(...))` → `lParam` on `WM_INITDIALOG`. The proc must `Box::from_raw` it once and stash per-dialog data in `DWLP_USER` (offset 16 on x64; we define it as a constant because the `windows` crate exports `DWL_USER` instead).
- `EndDialog(hwnd, rc)` closes a modal; never `DestroyWindow`.
- For tabbed dialogs use the **PropertySheet API** (`options.rs`), not a single dialog + `SysTabControl32` + show/hide panels. A property sheet wires each page as its own child dialog with an independent `DLGTEMPLATE` (see `dialog::build_propsheet_page_template`, flagged `WS_CHILD | DS_CONTROL`), so Ctrl+Tab cycling, tab ↔ page accessibility relationships, and per-page tab-order isolation come from the OS. Per-page commits run on `PSN_APPLY` via `DWLP_MSGRESULT = PSNRET_NOERROR`.

### History & "This PC"

- `History` (`navigator-gui/src/history.rs`) is a back/forward stack. `navigate` pushes unless `suppress_history` is set (back/forward set it before calling `navigate`).
- "This PC" is a sentinel `NavPath` (`NavPath::this_pc`, check with `is_this_pc()`). The scan worker routes it to `list_drives()` instead of `read_dir`. Navigating "up" from a drive root lands here.

### Config & persistence

- `ConfigHandle::load_or_default()` is infallible — a corrupt `config.toml` logs a warning and returns defaults.
- `config.toml`, `plugins/`, `clipboard.json`, `clipboard_history.json`, and `.trash/` all live next to the exe (or in the case of `.trash`, at each volume root). `navigator_config::exe_dir()` is the source of truth; don't hardcode.
- Sort mode, filters (show hidden/system), progress-window preference, shortcut bindings, hotspot slots, and per-column visibility (`general.columns`) all persist here.
- `Columns` defaults to all-on (Size/Type/Modified shown) so pre-existing configs keep the historical four-column view after upgrade. `SortMode::Type` was added alongside — sort works regardless of column visibility, so `type_key()` in `model.rs` is the source of truth and not the Type column label.
- **TOML can't hold `None` in arrays.** `hotspots` is stored as `Vec<String>` (empty string = unset), not `Vec<Option<String>>` — the latter serializes `None` and fails with `UnsupportedNone`. Hotspot slots must be exactly `HOTSPOT_COUNT` long; the code trusts the file to have the right length (no runtime padding), so a hand-edited short vec can panic — delete `config.toml` if it does.

### Hotspots

Ten numbered slots storing a `NavPath`. Two built-in actions per slot:

- `Hotspot{N}` (default `Ctrl+{N}`, slot 10 = `Ctrl+0`) — jump: navigate to parent and focus the entry. Empty slot announces "hotspot N empty" and does nothing.
- `HotspotSet{N}` (default `Ctrl+Shift+{N}`) — save the single selected entry into the slot, overwriting. Strict gate: exactly one selected row or prism announces the error count.

Jump reuses `AppState.pending_focus` + the existing `refocus_after_up` post-listing hook, so focus-by-filename works for any navigate, not just `navigate_up`. `AppState::jump_to(target)` is the one-shot helper for that path. Options → Hotspots tab shows slot contents and has Clear / Clear-all.

## Conventions

- **No `anyhow`** in the binary — `crates/navigator/src/main.rs` has a three-line `anyhow_lite` shim instead. Library crates use `thiserror`.
- **No cross-crate GUI leakage.** Anything touching `HWND` must stay in `navigator-gui`. `navigator-core` types cross the plugin ABI, so keep them OS-free.
- **`#![cfg(windows)]`** on crates that call Win32 directly (`navigator-gui`, `navigator-fs`, `navigator-prism`).
- Release profile uses `lto = "thin"`, `panic = "abort"`, `codegen-units = 1`, `strip = "symbols"`. Don't add `panic = "unwind"` dependencies without checking.

## Key bindings

See `README.md` for the user-facing table. User-bound actions live under `shortcuts` in `config.toml`; `navigator_config::shortcuts::default_actions()` is intentionally empty. The accel table is rebuilt on startup and on shortcut-editor save via `window::rebuild_accels`.
