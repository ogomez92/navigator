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

Personal install: `./r.sh` (Git Bash / WSL) or `r.cmd` (PowerShell / cmd) builds `--release` and copies `target/release/navigator.exe` to a personal bin dir as `x.exe`. Destination defaults to `~/stuff/bin/x.exe` (`%USERPROFILE%\stuff\bin\x.exe`) and is overridable via the `NAVIGATOR_INSTALL` env var. Both scripts then `rclone sync` the repo's `navigator_sounds/` into `<exe_dir>/navigator_sounds` — `sync`, not `copy`, so deleting a `.wav` from the repo folder also removes it from the install. That makes the repo folder the source of truth: a `.wav` dropped straight into the installed folder is wiped on the next build.

Runtime env: `NAVIGATOR_LOG` sets the `tracing` `EnvFilter` (default `info`).

### Native dependencies

- **Prism** (screen-reader / TTS C library) is linked by `crates/navigator-prism/build.rs`.
  - Resolution: `PRISM_DIR` env var, else the prebuilt prism **vendored in the crate** at `crates/navigator-prism/vendor/prism-windows-x64` (dynamic-only, ~5 MB, checked into git) — a fresh clone builds with no external setup. Only `prism.*` is vendored; tolk is not needed.
  - Expects `<base>/dynamic/<profile>/lib/prism.lib` + `<base>/dynamic/<profile>/bin/prism.dll`. Build copies `prism.dll` next to the output binary. `--features static` switches to `static/<profile>/lib`, which is **not** vendored — set `PRISM_DIR` to a full distribution for static builds.
  - `<profile>` is `debug` for dev builds, `release` otherwise.
- **rclone** must be on `PATH` at runtime — all file operations shell out to it.
- **7z** must be on `PATH` for the Extract action (Ctrl+E). Missing binary surfaces as a prism announcement, not a dialog. See `[extraction]` config + `extract.rs`.

## Architecture

Thin binary, fat workspace. `crates/navigator/src/main.rs` only parses args + initializes tracing, then calls `navigator_gui::run`. Everything else lives in sibling crates.

### Crates (dependency direction: top → bottom)

- **`navigator`** — binary entry point.
- **`navigator-gui`** — Win32 window shell. The only crate that knows about HWNDs. Owns `AppState`, the message loop, the virtual `SysListView32`, plugin host wiring, speech sink, file watcher, and worker threads.
- **`navigator-config`** — TOML config at `<exe_dir>/config.toml` (never `%APPDATA%`). `ConfigHandle` is an `Arc<RwLock<Config>>` clone-able handle. Also defines shortcut actions.
- **`navigator-plugin-api`** — stable C ABI for plugins. Plugins are `cdylib` crates exporting `navigator_plugin_entry`. Strings crossing the boundary are `*const u8 + len` (UTF-8), everything `#[repr(C)]`. Loaded with `libloading`.
- **`navigator-prism`** — safe FFI wrapper around the prism C library. `Prism` is a process-wide singleton guarded by an `AtomicBool`; `Speaker` handles are `Send` but not `Sync`.
- **`navigator-rclone`** — rclone driver. Spawns `rclone` with `--use-json-log --stats=1s --transfers N`, parses each stdout line as a structured log record. Emits `OpEvent::{Progress, Log, Done}` on a crossbeam channel; `Done` carries the exit code and a distilled `RcloneError` rather than raw log text (see *Error reporting* below). `conflicts()` diffs two `--dry-run` passes to find what a paste would destroy (see *Conflict handling* below). `RcloneDriver::with_transfers(n)` sets `--transfers`; `AppState::clone_for_worker` re-reads `config.rclone.transfers_clamped()` on every spawn so a config save applies to the next op without a restart. `base_args()` is exposed for tests.
- **`navigator-fs`** — directory scanning via raw `FindFirstFileExW` with `FindExInfoBasic` + `FIND_FIRST_EX_LARGE_FETCH`. Exposes `read_dir`, `list_drives` (for the virtual "This PC" view), and `search_recursive`.
- **`navigator-core`** — shared value types (`NavPath`, `Entry`, `Selection`, `Event`, `Error`, `ConflictMode`). No GUI / OS code; safe to use from plugins. `navigator-config` depends on it solely for `ConflictMode`, so the paste-conflict vocabulary has one definition instead of a mirrored enum that can drift.
- **`plugins/sample`** — example plugin.

### Threading model

One UI thread (the Win32 message loop) and several workers. All cross-thread comms go through `crossbeam-channel` or Win32 `PostMessageW`. Worker names below are thread names / modules inside `navigator-gui`, not separate crates.

- **`navigator-batch-op`** — per-paste worker. Runs `run_batch`: resolves the conflict mode, then issues one rclone invocation per source folder for files (see *Batching*) plus one per directory.
- **`navigator-scan`** — long-lived worker. Handles `ScanCmd::List` (directory scan) and `ScanCmd::Search` (recursive search). Posts results back as `WMAPP_DIR_LISTED` / `WMAPP_SEARCH_RESULTS`. **`ScanCmd::List` carries the current `Sort` and the worker sorts before posting** — see *Keeping work out of the message pump*.
- **`navigator-trash-survey`** — sizes every drive's `.trash` for the empty-trash confirmation, then posts `WMAPP_EMPTY_TRASH_SURVEYED`. The UI thread only shows the dialog and spawns the deleter.
- **`navigator-shell-op-reaper`** — waits on the detached shell-copy child purely to speak the outcome and refresh. Not what keeps the copy alive; see *Detached shell copy*.
- **`navigator-plugin-nav`** — bridges plugin nav requests into `AppState::navigate` via a weak `Arc` so it dies with the app.
- **`navigator-rclone-op` / `navigator-batch-op` / `navigator-batch-delete`** — short-lived per-operation threads. They hold a `WorkerCtx` (cheap clone of rclone driver, speech sender, scan sender, optional progress handle) — never borrow `AppState`.
- **Speech sink** — `SpeechSink` owns its own thread; everything (plugins, workers, UI) just sends `Utterance` messages.
- **Sound player** — `SoundPlayer` (`sound.rs`) owns its own thread; same shape as the speech sink but carries a `ConfigHandle` so it can resolve `SoundEvent` → filename per call. Cheap to clone, so it lives on `AppState` *and* on every `WorkerCtx`.
- **File watcher** — `notify::RecommendedWatcher` in `AppState.watcher`. Replaced on each navigation; dropping unsubscribes.

### Keeping work out of the message pump

Spawning a worker is only half the job — the *setup* a command does before it spawns runs in the window procedure, and several commands were doing unbounded IO there. The rule: **anything whose cost scales with the selection, the clipboard, or the size of a directory belongs on a worker.** Each of the following was a measured freeze, not a theoretical one.

- **The watcher stats one file, never the directory.** `single_entry` calls `navigator_fs::stat_entry` — one `FindFirstFileExW` on the exact path. It used to `read_dir` the parent and search the result for the name, which made each watcher event cost O(entries in the folder); a paste or extract of N files into a folder of M entries was N×M work *in the pump*, and a few thousand of each froze the window for the whole operation. Never turn this back into a scan-and-find.
- **The scan worker sorts.** `ScanCmd::List` carries a `Sort`; the worker sorts and the payload is `(path, entries, sort)`. `Model::set_listing_presorted` installs the vector untouched when the tag matches the live preference and re-sorts when it doesn't (the user can change sort mode while a scan is in flight). `Model::set_sort` therefore only *records* the preference — every caller follows it with a `refresh()`, and sorting in both places did the same n log n twice, once in the pump.
- **`sort_entries` decorates once per entry, not once per comparison.** Name/Type need a case-folded key; building it inside the comparator meant two `String` allocations on each of the n log n comparisons. It now folds up front and sorts an index permutation (so `Entry` values move once, via `apply_permutation`). `fast_sort_matches_the_reference_comparator_for_every_mode` pins the result against the old comparator — this is a speed change and must not alter a single row's position.
- **Paste resolves its undo targets on the worker.** The "did this destination already exist?" filter is one stat per clipboard entry and it *must* run before anything is written (afterwards, the paste's own output answers yes). It also must not be skipped — see the undo rule under *Clipboard + undo + trash*. So `op_paste` pushes an empty `PastePlan` onto the undo stack before spawning (preserving "Ctrl+Z can reach an in-flight paste") and `run_batch` publishes the filtered pairs as its first act. A Ctrl+Z landing in that gap is told to retry rather than handed an unfiltered list, which would delete exactly the files the conflict mode protected.
- **Delete names trash dirs on the UI thread and creates them on the worker.** `trash_dir_on_volume_of` is pure path arithmetic; `run_trash_batch` does the `create_dir_all` immediately before the rename that lands in it. Creating them up front was one syscall per selected item in the pump, before the delete had even started.
- **Empty trash surveys on a worker.** Sizing every drive's `.trash` is an unbounded recursive walk. `op_empty_trash` spawns `navigator-trash-survey`, which posts `WMAPP_EMPTY_TRASH_SURVEYED` with `(dirs, body, total)`; `confirm_empty_trash_survey` runs the dialog and the deleter.
- **`extract::find_7z` is a `OnceLock`.** It stats every directory on `PATH`, and both callers run it on the UI thread just to decide whether to report "7z not found" before spawning. 7-Zip does not move mid-session.
- **`push_history` is queued onto a `navigator-history` thread.** It is a read-modify-write of the *whole* `clipboard_history.json`, and each of the 20 retained entries carries the full source list of its operation — so after a few 50k-file copies, every subsequent Ctrl+C was parsing and re-serialising millions of path strings in the pump. One dedicated thread (not `thread::spawn` per call) keeps writes ordered. `save_clip` stays synchronous: a paste reads it immediately afterwards. `run` calls `clipboard::flush_history` after the message loop, because `main` exits via `std::process::exit` and would otherwise kill the writer mid-queue on a copy-then-quit.

Still on the UI thread by choice: `op_restore_from_history` stats one history entry's paths so it can announce the missing count in the same breath — bounded by a single operation, and moving it would split the announcement away from the command.

### Virtual ListView

The main control is `SysListView32` in `LVS_OWNERDATA` (virtual) mode. The backing store is `Model` in `navigator-gui/src/model.rs`. Row data is pulled on-demand via `LVN_GETDISPINFO`, so million-entry folders render instantly. Mutations don't rebuild the control — they update `Model` and send targeted `LVM_REDRAWITEMS` (`WMAPP_REDRAW_ROW`).

**Column visibility is dynamic.** `Name` always shows at iSubItem 0; `Size`/`Type`/`Modified` are toggled in `config.general.columns`. `ListView::create` walks `listview::visible_columns(&cols)` to insert only enabled columns, so the physical iSubItem indices are a *prefix* of the logical enum. `fill_dispinfo` must go through `listview::column_for_subitem(&cols, sub)` to recover the `LogicalColumn`; indexing with the old hard-coded `COL_NAME`/`COL_SIZE`/… constants is wrong once a column is hidden. Options → Columns commits via `AppState::reconfigure_listview_columns`, which posts `WMAPP_RECONFIGURE_COLUMNS` so the UI thread tears down + re-inserts columns and refreshes the virtual count. Sort keys (`SortMode::Type` included) are independent of column visibility: you can sort by Type with the Type column hidden.

### Input handling (landmines)

The Win32 input pipeline has two stages in the message pump and several interacting sources of truth. Getting these wrong silently breaks shortcuts.

- **Pump order is accelerators → IsDialogMessageW → TranslateMessage/DispatchMessage** (Petzold order). Reversing the first two lets `IsDialogMessageW` swallow `Ctrl+letter` / `Alt+letter` chords before the accel table sees them — every user-configured shortcut and every static `Ctrl+C/X/V/A/H/F` binding dies silently. Keep accel first.
- **Accel table deliberately omits VK_BACK, VK_DELETE, VK_RETURN.** Those are scoped to the listview via `SetWindowSubclass` in `crates/navigator-gui/src/listview.rs`, which posts `WM_COMMAND(Commands::{Back,Delete,OpenFocused})` to the parent only when the listview actually has focus. If those keys were in the global accel they would fire from inside the address-bar edit too, breaking editing (Backspace navigates up, Delete deletes the selection, Enter fires IDOK).
- **Enter routing.** `IsDialogMessageW` turns `VK_RETURN` into `WM_COMMAND(IDOK)` when no default button exists. The IDOK arm in `handle_command` routes by `GetFocus()` — listview → `open_focused`, address → `navigate_from_address`. Don't blindly call `navigate_from_address` on IDOK; that was the previous bug.
- **Model is source of truth for selection/focus.** `LVN_ITEMCHANGED` must be mirrored into `Model.selection` via `mirror_item_change` (diffs `uOldState` / `uNewState`). Without the mirror, `selected_paths()` / `focus()` are empty, so `op_copy` / `op_delete` / `open_focused` / `run_action` all no-op. `Selection::insert` / `remove` are idempotent for exactly this path; `toggle` flips and is wrong here.
- **Range multi-select needs a second notification.** Virtual (`LVS_OWNERDATA`) listviews fire `LVN_ODSTATECHANGED` (one `NMLVODSTATECHANGE` with `iFrom..=iTo`) for shift-click / shift-arrow / Ctrl+A, *not* per-row `LVN_ITEMCHANGED`. `mirror_range_change` handles it. Without that arm, multi-select rows never make it into `Model.selection` and every batch op says "nothing selected". **Do not apply the reported `iFrom..iTo` delta incrementally** — a multi-row gesture that crosses the anchor (Shift+Home/End, Shift+PageUp/Down, shift-click to the other side) deselects one block and selects another in one shot, and `insert`/`remove` over the single reported range leaves far-side rows stuck → the model selection drifts larger than the control's and looks "inverted". `mirror_range_change` instead calls `rebuild_selection_from_control`, which re-derives the model's selected set from the control via `LVM_GETNEXTITEM`/`LVNI_SELECTED` (the OS owns authoritative item state for owner-data). Single-row shift-arrow stays on the cheap `mirror_item_change` delta path.
- **Refocus after navigate-up.** `AppState.pending_focus: Mutex<Option<NavPath>>` stores the child path before `navigate_up` fires. The post-listing hook (`refocus_after_up` in `window.rs`) consumes it and calls `select_row`. For drive-root → This PC it matches via `navigator_fs::drive_path_from_display` inverse; for regular folders it matches by filename.
- **WM_SETFOCUS on the main hwnd redirects to the listview.** Without it Windows parks focus on the first tabstop (address bar) after first-show and alt-tab-back. Listview is what the user wants 99% of the time.
- **UNC bare shares.** Rust's `Path::is_absolute` returns `false` for `\\host\share` (prefix without root component). `NavPath::new` retries with a trailing `\` so IP-based shares like `\\100.86.173.34\media` navigate.
- **Default shortcuts are populated on first run only.** `default_actions()` seeds Copy / Cut / Paste / F2 / F5 / Hotspots etc. when no `config.toml` exists. There is intentionally **no migration chain** — this is a single-user tool, and if a stale config ever drifts from current defaults we delete `config.toml` and regenerate. Don't add a migration helper; just ship a breaking default change and expect the user to re-run.
- **Listview needs `LVS_EDITLABELS` for F2.** `LVM_EDITLABELW` is a silent no-op without that style. `LVN_BEGINLABELEDITW` / `LVN_ENDLABELEDITW` route the result to `op_rename`.
- **We drive type-ahead, not `SysListView32`.** The control's private prefix buffer is not clearable externally — a letter typed after navigating into a new folder would resume the old buffer. `AppState.type_ahead: Mutex<(String, Instant)>` is our own buffer; the listview subclass consumes `WM_CHAR` (returns `LRESULT(0)`) so the control never accumulates, and `AppState::reset_type_ahead()` runs at the top of every `navigate`. `type_ahead_step(ch)` appends + searches via `model.find_prefix`, auto-resetting after a >1s gap for Explorer-cadence.

### Archive extraction (Ctrl+E)

`extract.rs` shells out to `7z.exe` on `PATH`. Pure helpers (`is_extractable`, `parse_top_level_count`, `archive_stem`, `decide_dest`, `unique_dest`) are kept separate from `run_extract` so the wrap-folder decision and extension classification are unit-testable without spawning a process. `EXTRACTABLE_EXTENSIONS` is the source of truth — extend it, don't pre-filter elsewhere.

`AppState::op_extract` filters the selection to extractable + local (remote rclone paths are skipped — 7z can't read `\\?\NavigatorRemote\...`), then validates `find_7z()` before spawning the worker. Two error paths announce via prism: "no extractable archives selected" and "7z not found on PATH".

Wrap-folder rule (`decide_dest`): if `[extraction] create_folder = false` OR the archive already has ≤1 top-level entry → extract straight into the parent (no `name/name/...` double); otherwise wrap in `parent/<archive_stem>` deduped by `unique_dest`. `archive_stem` strips layered extensions so `foo.tar.gz` → `foo`.

**Both 7z calls (`l` and `x`) MUST set `CREATE_NO_WINDOW`** via the local `no_console` helper. Without it 7z pops a console that steals focus from the listview every archive — same flag the rclone driver uses.

The Extract worker deliberately does NOT call `state.refresh()`. The notify watcher already folds new files into the listing via `Model::append_entries`, which keeps existing sort order and lands fresh entries at the bottom — provided `general.new_items_at_bottom` is true (the default; guarded by `new_items_at_bottom_default_is_on`). Don't add a refresh; it would re-sort and lose the user's anchor.

`opts.delete_when_extracted` only deletes on `7z x` exit-success. Failures keep the archive.

### File operations invariant

All mutations (copy, move, delete, rename) go through `navigator-rclone`. No direct `DeleteFileW`.

Two deliberate exceptions, both routed through `shell_op.rs`:

- **Ctrl+Alt+V** (paste from the OS clipboard) hands the batch to the Windows shell copy engine on purpose — the shell is what antivirus recognises as a legitimate file operation, so a paste of thousands of files avoids the heuristics that flag rclone streaming them.
- **Delete on a UNC share** goes to the shell's delete engine, because navigator's own delete stages to a `.trash` directory on the target volume and a network share is not ours to litter. See *Clipboard + undo + trash*.

Everything else — including every rclone-clipboard paste and every local delete — stays on the rclone path.

### Detached shell copy/delete (`shell_op.rs`)

`SHFileOperationW` is synchronous and it used to be called straight from the window procedure, which meant the message pump was blocked for the whole transfer *and* the transfer died with the process — closing navigator mid-paste killed a copy that might be hours from done.

It now runs in a **separate process**: `spawn_detached` re-executes navigator's own binary as `navigator --shell-op copy|move --dest <dir> --list <file>`, and `navigator_gui::try_run_shell_op` (called from `main` **before** anything else, so no config is read and no HWND is created) runs `run_helper` and exits. The parent is free the moment `CreateProcess` returns. Quitting navigator mid-paste now leaves the shell's progress dialog running, exactly like closing an Explorer window after starting a copy from it.

- **`ShellVerb::Delete` rides the same machinery for an unrelated reason.** It is the UNC-share delete path (see *Delete on a network share* below), not an antivirus concern — it takes no `--dest`, and `parse_helper_args` *rejects* one rather than ignoring it, because a crossed command line would otherwise turn "copy these somewhere" into "delete these". `shell_delete` passes `FOF_ALLOWUNDO`: on a share, which has no Recycle Bin, that is what makes the shell put up its own "are you sure you want to permanently delete" prompt instead of deleting silently. Answering No comes back as a clean return with `fAnyOperationsAborted` set — i.e. `EXIT_ABORTED`, indistinguishable from a mid-run Cancel and not a failure either way.
- **Sources travel through a temp file, not argv.** A few thousand paths blow past the 32 KB command-line limit and the failure mode of a truncated list is a silent partial copy. The list is newline-delimited (Windows forbids `\n` / `\r` in filenames); `encode_list` *drops* a path containing one rather than emit a list that decodes into a path nobody selected. The **child** deletes the list file — the parent is long gone by then.
- **`CREATE_BREAKAWAY_FROM_JOB` is attempted, then retried without it.** If navigator was itself started inside a job object that kills children on close (some terminals and task runners), the helper would inherit the job and die with us, defeating the whole point. Jobs that forbid breakaway make `CreateProcess` fail, hence the retry.
- **`CREATE_NO_WINDOW`** — the debug build is a console-subsystem binary and would otherwise flash a console that steals focus from the listview on every paste. Same flag the rclone driver and 7z calls use.
- **`AllowSetForegroundWindow(child.id())`** hands the child our foreground right. The shell's progress and overwrite dialogs belong to that process; without this they can come up *behind* navigator, and an overwrite prompt the user never sees is an operation that looks hung.
- The child gets `CoInitializeEx(APARTMENTTHREADED)` — the shell engine wants COM on the calling thread.
- Exit codes are the contract: `0` ok, `1` failed, `2` user cancelled in the shell's dialog, `3` bad args. The parent's reaper thread maps them to a spoken summary and a final `refresh()`. **The reaper is not what keeps the copy alive** — the child owns itself, so if navigator exits first the copy simply finishes unannounced. A malformed `--shell-op` must exit `3`, never fall through to the GUI: opening a file explorer because a flag was missing would leave the user staring at a window they didn't ask for while the paste silently never happened.
- Detached ops deliberately do **not** hold an `OpGuard`, so they don't count toward `ops_in_flight` and the close-confirmation ignores them. That's correct — they survive the close.

### Conflict handling is mode-based, not per-item

There is **no per-file "this exists — replace it?" prompt**. rclone already knows how to compare two trees, so a paste carries a `ConflictMode` (in `navigator-core`, shared by config and rclone) and the question is asked once per batch, if at all:

| Mode | rclone | Destructive? |
|---|---|---|
| `AddNewOnly` | `--ignore-existing` | no |
| `Update` (default) | `--update` | yes |
| `Replace` | `--ignore-times` | yes |
| `Mirror` | verb becomes `sync` | yes, **deletes unselected destination files** |

`Mirror` is the only mode that changes the verb; the rest are pure flags. A `Move` can never mirror (rclone has no verb that both prunes the destination and empties the source), so `op_args` degrades `Move` + `Mirror` to `Replace` — defined rather than surprising, and the UI hides Mirror for a cut clipboard anyway.

**Ctrl+V only prompts when data would actually be lost.** `WorkerCtx::resolve_conflicts` (a worker method — it never borrows `AppState`) short-circuits hard, in this order: a non-destructive mode never asks; then `preflight::conflict_candidates` narrows the set, and if it comes back empty it returns without spawning rclone at all (the common case — zero dialogs, zero dry-runs). Only genuinely colliding items get a `RcloneDriver::conflicts` call. If that comes back empty (everything identical, or protected by `--update`'s newer-destination guard) the paste still runs silently.

**`conflict_candidates` is local-vs-remote aware, and this matters.** For a local destination it is `top_level_conflicts` — a cheap `exists()` filter. For a **remote** destination it returns *every* source, because a remote `NavPath` is a synthetic `\\?\NavigatorRemote\…` string for which `Path::exists()` is always false. Using `exists()` to pre-filter a remote destination meant no candidates → no dry-run → **no conflict dialog ever** when pasting onto a remote, even though the two-pass diff itself is backend-agnostic. Don't reintroduce a bare `exists()` on that path.

**Keep both is offered only for local destinations.** `unique_numbered_path` can only probe the local filesystem; on a remote it returns the name unchanged and the item quietly degrades to an additive copy (a skip) instead of keeping both. Both dialogs gate it on `!dest_dir.is_remote()`.

**Undo may only delete destinations that did not exist before the paste.** `op_paste` filters `created`/`originals` as aligned pairs before pushing `UndoAction::Paste`. Every mode can decline to write an existing destination — `AddNewOnly` skips it, `Update` spares a newer one, `Replace` overwrites it with no backup, `KeepBoth` writes a numbered sibling — so an unfiltered `dest.join(name)` list made Ctrl+Z delete exactly the files the mode had protected, unrecoverably. Guarded by `undo_targets_exclude_preexisting_destinations`.

**`RcloneDriver::conflicts` runs two `--dry-run` passes and diffs them** (`op::victims`): the caller's mode, then the same op forced to `AddNewOnly`. Anything in the first but not the second was excluded *purely because the destination exists* — that is the definition of a conflict. Diffing rather than probing the filesystem keeps it backend-agnostic: it works for a remote destination with no `lsjson` round-trip and no path arithmetic. `AddNewOnly` short-circuits without spawning.

**Drain both of `preflight`'s pipes concurrently.** It reads stderr on the calling thread and stdout on its own; reading one to EOF and *then* the other deadlocks. rclone's JSON log goes to stderr and stdout stays empty until exit, so a sequential reader parked on an empty stdout while the child filled the 64 KiB stderr pipe buffer, the child blocked on a write nobody was reading, and the paste worker hung forever having just announced "checking destination". It looked intermittent because it is a volume threshold, not a logic error: one ~235-byte record per file (≈280 files) *or* a ~510-byte `--stats 1s` line every second, so a slow remote listing reaches it on timing alone with an empty destination and no files at all. `preflight_does_not_deadlock_when_the_dry_run_floods_stderr` pins it. `spawn` always used two reader threads; keep the two in the same shape.

**Parse the `skipped` field, never `msg`.** rclone renamed the dry-run text from `Would copy` to `Skipped copy as --dry-run is set`, which silently broke the old substring match — `PreflightReport.would_overwrite` was permanently empty for releases. The structured `skipped` field (`"copy"` / `"move"` / `"delete"`) has been stable; `LogEvent::is_dry_run(verb)` wraps it. `sync --dry-run` reports prunes as `skipped: "delete"`, which is how Mirror's confirm lists what it would delete for free.

**Keep both is deliberately coarse.** It renames the *selected* item — pasting `photos` onto an existing `photos` yields `photos (1)`, it does **not** number files inside a merged tree (near-impossible to undo or reason about). `preflight::top_level_conflicts` is what it acts on, and `unique_numbered_path` picks the sibling name (Explorer parity — `archive.tar.gz` → `archive.tar (1).gz`; extensionless names and directories just get ` (1)`). For copy paths the batch worker uses `Operation::CopyTo { src, dst }`; for cut paths `Operation::Rename { src, dst }`. `CopyTo` is distinct from `Copy { dest_dir, .. }` because `Copy` always keeps the source filename — don't shove a renamed destination through it. Non-colliding items in a Keep-both batch run as `AddNewOnly`, so that path cannot touch anything.

**Ctrl+Shift+V (Paste special)** always asks, via a radio-group TaskDialog listing every mode plus Keep both. It's the only route to Mirror. A preset choice suppresses the after-the-fact confirm — the user already chose explicitly, so don't ask twice. Detection failure is **not** treated as "no conflict": `resolve_conflicts` falls back to naming the colliding top-level items so the user still confirms.

The confirm dialog defaults to the *safe* button (`Add new only`), so an absent-minded Enter cannot destroy anything. `preflight::paste_summary` names the mode in the spoken summary — "done — 8 items, add new only" explains why a paste that looked like it should have changed something didn't.

### Batching: one rclone invocation per source folder

`Operation::Copy` reads only `sources.first()`, so a paste used to be one process per item — 200 files meant 200 sequential spawns, and `--transfers N` had a single file per process, i.e. nothing to parallelise. Measured through the driver: **28.98 s per-item vs 0.26 s batched, 112× ­**, essentially all of it process startup rather than I/O.

`navigator-gui/src/batch.rs` splits a selection into `FileGroup`s (files sharing a parent → one `Operation::CopyBatch`/`MoveBatch` with `--files-from`) and singles. `partition` takes `is_dir` as a closure so it is pure and unit-testable with no filesystem. Both the transfer (`run_group`) and conflict detection (`resolve_conflicts`) batch — detection is two dry-run spawns per op, so overwriting 200 files would otherwise cost 400 spawns before a byte moves.

Three hard rules, each with a test:

- **Directories must never enter a `--files-from` list.** rclone reads a directory entry, transfers nothing, warns about nothing, and **exits 0** — a folder routed through a list is silently lost while the paste reports success. `files_from_silently_ignores_directories` in `navigator-rclone/tests/driver.rs` pins that rclone behaviour and tells you to revisit the partition rule if it ever changes.
- **Mirror must never batch.** `sync --files-from` considers only the listed names and prunes everything else at the destination — an unannounced mass delete of files the user never selected. Callers gate it out; `op_args` additionally degrades a batched Mirror to Replace so a slip costs an overwrite, not a wipe (`batch_never_emits_sync_for_mirror`).
- **The verb is `copy`/`move`, not `copyto`/`moveto`.** With `--files-from`, listed names reproduce directly under `dest_dir`; `copyto` would treat the destination as one target path and bury everything a level deep (`batch_uses_copy_not_copyto`).

Names containing `\n`/`\r` fall back to singles — the list is newline-delimited, and a corrupted entry fails *silently* per rule one. Windows forbids those characters; rclone remotes need not.

`run_group` verifies every expected destination exists afterward when the destination is local, because exit code 0 does not mean the files arrived. A remote destination skips the check (probing costs an `lsjson` round-trip per group, giving back the latency this path exists to remove) and trusts the exit code. `TempList` owns the list file and deletes it on drop; `BATCH_LIST_SEQ` keeps concurrent pastes from sharing a filename.

Keep-both never batches — each item needs its own renamed destination.

### Progress reporting: the job, not the invocation

**A user action is not an rclone invocation, and progress must be reported at the action level.** Batching broke this: a paste used to narrate its own item loop (`"1 of 200: a.txt"`, `"2 of 200: b.txt"`, …), and collapsing 200 items into one `--files-from` call deleted the loop and every utterance with it. Reporting per invocation is equally wrong the other way — a three-folder paste would run 0–100% three times.

`navigator-gui/src/narrate.rs` owns the model, and is pure + unit-tested:

- **`Meter`** aggregates N invocations into one monotonic 0–100%. Each invocation declares a **weight in job units** (a `--files-from` group weighs `names.len()`, a directory weighs 1, a trash-rename weighs 1) and contributes `weight × its own fraction`. `set_fraction` only ever moves forward, because rclone's `totalBytes` grows while it scans and a raw fraction dips. `percent()` divides the *unfloored* total, so a one-unit job (a single big file) still climbs; `units_done()` floors, so it only claims finished items.
- **`Cadence`** decides *when*. Nothing at all for the first interval — a copy that finishes inside it says only its summary — then at most one utterance per interval, and never the same sentence twice running, so a stalled transfer goes quiet and resumes the moment the numbers move.
- **`phrase`** returns `None` at 100%: every caller already follows completion with a summary (`"done — 200 items, update"`), and rclone's closing stats tick would otherwise squeeze `"100 percent, 200 of 200"` in just ahead of it.

`OpProgress` in `app.rs` is the impure driver: one per user action, holding the meter, the cadence, the progress-window handle and the cancel flag. `WorkerCtx::run_op(op, &mut prog)` runs a single invocation against it; `run_one` is the one-shot wrapper that builds a 1-unit job for renames / mkdir / touch. Callers bracket each invocation with `prog.begin(weight)` / `prog.end()`, account for items they decline to run with `prog.skip(n)` (otherwise the percentage stalls short of 100), and call `prog.finish(ok)` **once** at the end.

**`run_op` must not post completion to the progress window.** It used to: `post_done` fired per invocation, so the first group of a multi-group paste flipped the window to "Done." and disabled Cancel while the rest were still running. Completion belongs to the job.

Speech and window carry deliberately different detail. Speech is terse (`"45 percent, 90 of 200"`) — it's spoken over whatever the user is doing, and the filename is exactly what batching exists to stop announcing. The window gets counts, bytes, rate and ETA, plus the in-flight filename, plus the percentage in its **caption** so a screen reader's read-title command answers "how far along?".

**The in-flight filename comes from `stats.transferring[0].name`, not `object`.** rclone's stats records carry no top-level `object` — that appears only on the per-file INFO records, which fire when a transfer *ends*. Reading `object` off a stats record left the window's "Current:" line permanently blank. `a_real_rclone_stats_line_parses_end_to_end` pins a verbatim 1.73.5 record against both facts.

Cancel works now: `OpHandle::canceller()` hands out a `Send + Clone` kill switch, `OpProgress::arm_cancel` re-installs it per invocation, and the flag is checked between items so cancelling a 200-item paste stops the paste rather than one file. A cancelled child exits non-zero — `run_op` returns early on `prog.cancelled()` so that never becomes an error dialog or a UAC retry prompt.

`general.announce_interval_secs` defaults to **5**, not 0. Per the no-migration rule below, an existing `config.toml` keeps whatever it has — set it in Options → Speech or delete the file. `0` still means "no periodic speech"; the completion summary and the window are unaffected.

### Error reporting: the job, not the invocation either

Same rule as progress, for the same reason, and it was broken in both directions.

**A failure is not a log record.** rclone answers `--use-json-log` with a *stream* of them, and the sentence the user needs is the tail of the last one. Deleting a missing file emits five records — three of them the same retry — each carrying a timestamp, a Go source location and an `objectType`. The old code took the last ten stderr lines and put them in a `MessageBoxW`, so the dialog was a wall of JSON and a screen reader read out a timestamp and `slog/logger.go:256` before reaching "object not found".

`navigator-rclone/src/error.rs` distils that stream. `ErrorCollector` watches every record; `finish(exit_code)` produces an `RcloneError` with the real message (wrappers peeled), an `ErrorKind` the UI phrases itself, and the supporting lines kept apart for the details block. `OpEvent::Done` carries it — `stderr_tail: String` is gone, and with it every consumer that was showing or speaking raw log text.

- **The end-of-run verdict is logged at NOTICE**, not ERROR. `Failed to copyto: directory not found` is the one line that names the operation and survived all the retries, and filtering on level alone throws it away. `ErrorCollector` keeps a record when it is `critical`, `error`, *or* starts with `Failed to `.
- **The verdict lives in its own slot, not in the capped detail list.** rclone logs a per-object failure as `Failed to copy: …`, which is verdict-shaped, so exempting verdicts from the cap would leave the cap unenforced by a failure storm. `the_details_pane_is_capped_and_says_so` pins it — and the pane says how many lines it dropped, because silent truncation reads as "that was everything".
- **A configuration failure never retries**, so it has no verdict at all: one `critical` record is the whole report (`Failed to create file system for destination "x:": didn't find section in config file`). That is where `ErrorKind::NoSuchRemote` and the object name come from.
- **Both reader threads are joined before `Done` is sent.** `child.wait()` returns while the readers are still draining buffered pipe data, and the error we want is the *last* thing rclone wrote. The previous code also fed stderr into a `bounded(1024)` channel with `try_send`, which drops the *newest* line when full — so a long failing op kept its first 1024 lines and threw away the tail it was named for.
- **Exit codes classify what the text doesn't.** rclone documents 3 as directory-not-found and 4 as file-not-found; both are pinned end-to-end in `driver.rs` against a real binary.
- **`reason()` and `summary()` are not interchangeable.** `summary()` folds the object into the sentence, for callers that speak a failure alone (remote download / upload / delete). `narrate::Failure` carries the subject in its own field and so takes `reason()` — using `summary()` there names the item twice *and* breaks grouping, since two files failing the same way would look like two different reasons.

The GUI half is `OpProgress`, which collects failures and reports them **once**, from `finish`. Each invocation used to open its own modal dialog: a five-item delete of files that were already gone put up five identical dialogs, each blocking the worker until dismissed, and a batched paste interrupted itself mid-job while later groups were still running. `narrate::failure_report` (pure, unit-tested) groups identical reasons — "Delete failed for 3 of 5 items: not found" — and lists which items under each. A cancelled job reports nothing; the failures it collected are the user's own doing.

Failures the app detects itself go through `OpProgress::record_problem`, and must: a batch that exits 0 with destinations missing (rclone silently ignores a directory in `--files-from`) has no rclone error behind it, and without that call the job closes clean while the files never arrived.

The elevated retry path can't pipe, so it redirects rclone to `--log-file`; `RcloneError::from_log_text` runs that through the same distiller. It reports via `record_elevated_failure`, which appends "even as administrator" — a failure the user just approved a UAC prompt for must not come back reading "permission denied". UAC escalation still keys off `probe_write_access`, **not** `ErrorKind::PermissionDenied` — rclone's message is locale-translated and lies on protected-root writes (it says "file not found" for an ACL denial), which is exactly why the probe exists.

### Clipboard + undo + trash

- **Clipboard is file-backed**, not the Windows clipboard. `<exe_dir>/clipboard.json` holds `{sources, cut, ts}`; written by copy/cut/append, read by paste. Two running navigator instances share it automatically. The OS clipboard is untouched except by `op_copy_paths` (CF_UNICODETEXT on purpose).
- **Operation history** lives in `<exe_dir>/clipboard_history.json`, capped at `MAX_HISTORY` (20) rolling entries. Feeds File → Recent operations; `WM_INITMENUPOPUP` rebuilds the submenu each time from disk so peer instances' writes show up. Command IDs `Commands::RecentOpsBase..+20` route clicks to `op_restore_from_history`.
- **Undo stack is in-memory only** (`AppState.undo_stack: Mutex<Vec<UndoAction>>`, capped at 50). Variants: `ClipChange { prev }` (reverse copy/cut/append/restore) and `Paste { plan, cut_mode }` (copy-undo deletes the plan's `created`, cut-undo moves each back to its `originals[i]`) and `Delete { pairs: Vec<(trash, original)> }`. Push happens *before* spawning the worker so Ctrl+Z can reach even in-flight operations. `Paste` therefore pushes an **empty `PastePlan`** that `run_batch` fills in as its first act (the filter behind it is one stat per clipboard entry — too much for the UI thread); `op_undo` on an unpublished plan re-pushes it and says "try again in a moment" rather than reverting a superset.
- **Paste arms `pending_focus`.** `run_batch` captures the first successfully created destination and calls `AppState::set_pending_focus` before `refresh()`, so `refocus_after_up` lands the caret on the pasted row by filename. Same pattern as undo-delete; relies on the weak `Arc<AppState>` in `WorkerCtx.state`.
- **Delete splits three ways by endpoint.** Remote (rclone) → confirm + `purge`/`deletefile`, no undo. **UNC → the Windows shell** (below). Local → trash + undo. A selection can mix all three; each half runs its own path.
- **Delete on a network share never touches `.trash`.** `volume_root_of` is `path.ancestors().last()`, which resolves `\\host\share\dir\file` to `\\host\share\` — so the trash rename *worked* on a UNC path, by creating a `.trash` directory at the root of somebody else's file server. Worse, macOS and Samba SMB servers both map dot-prefixed names to `FILE_ATTRIBUTE_HIDDEN`, so `Model`'s hidden filter then hid the trash from the very user looking for their file: it read as "the delete silently ate it". `op_delete` now routes `NavPath::is_unc()` targets to `spawn_shell_delete` → the detached `--shell-op delete` helper, which gives Explorer's behaviour *and* Explorer's confirmation prompt (hence no `MessageBoxW` of our own — asking twice trains the user to Enter through dialogs). No undo entry: nothing of ours holds the file. `trash_dir_on_volume_of` additionally returns `None` for a UNC path so a future caller can't reintroduce the litter by forgetting to branch; `trash_dir_is_never_named_on_a_unc_share` pins it.
- **`NavPath::is_unc()` must exclude the sentinels.** `\\?\NavigatorThisPC`, `\\?\NavigatorRemotes` and `\\?\NavigatorRemote\…` all start with two backslashes; a sentinel is not a network location. `\\?\UNC\host\share` is one, and `\\.\` (device namespace) is not.
- **Delete → trash, not purge (local only).** `op_delete` renames each target to `<volume_root>/.trash/<unix_ts>_<counter>/<basename>` on the *same* drive (derived via `volume_root_of`) so the move is atomic — no cross-drive copy. The worker is `run_trash_batch`; undo is `run_revert_delete`, which skips targets whose original path is now re-occupied rather than clobbering. On successful undo the worker arms `AppState::set_pending_focus` with the first restored path, so the subsequent refresh lands the caret back on the recovered row — the worker reaches into `AppState` via the `Weak<AppState>` stored on `WorkerCtx.state` (set up in `AppState::new` via `self_weak: OnceCell<Weak<Self>>` so methods on `&self` can still hand workers a route back). Trash is never auto-purged; closing the app orphans the undo handle but the staged files remain.
- **Clipboard path validity is checked at paste/restore, not at copy.** `op_paste` filters sources via `NavPath::new`; `op_restore_from_history` partitions by `Path::exists()` and announces missing-count without touching the clip file if *all* paths are gone.

### Event sounds

`SoundEvent` (in `navigator-config/src/sounds.rs`) enumerates the ~19 events a
`.wav` can be attached to; `Sounds` maps `event key -> filename` and lives at
`[sounds]` in `config.toml`. Files are read from `<exe_dir>/navigator_sounds`
(`SOUNDS_DIR_NAME`) — nothing is bundled, so a fresh install is silent.

- **The map is keyed by string, not a struct field per event.** An assignment
  naming an event we later rename (or one a newer build added) is ignored
  rather than failing the whole `config.toml` parse and taking every other
  setting down with it. `SoundEvent::key()` is therefore an on-disk contract —
  `keys_are_stable` pins the ones that shipped.
- **`sound_path` only accepts a bare filename.** Anything containing a
  separator or `..` is rejected, so a hand-edited config can't aim the player
  at an arbitrary file. Don't "helpfully" join raw config strings onto
  `sounds_dir()` elsewhere.
- **Playback is `PlaySoundW` + `SND_ASYNC` on a dedicated thread.** That API is
  a single process-wide channel: a new sound stops the previous one, which is
  what you want for event feedback (the user always hears the most recent
  thing). The thread exists because `PlaySoundW` still parses the WAV header
  synchronously — a file on a cold or network drive would otherwise stall the
  UI thread — and because the UTF-16 filename must outlive the call.
  `SND_NODEFAULT` means a missing file is silent instead of a system beep.
- **Outcome beats verb.** Workers call `WorkerCtx::play_outcome(cancelled,
  failed, done)`, never `play(done)` directly, so a failed or cancelled copy
  can't chime like a successful one.
- **`AppState.next_nav_sound` is how navigation events stay distinguishable.**
  Every route into a folder funnels through `AppState::navigate`, so
  `navigate_up` / `go_back` / `go_forward` arm the cue immediately before
  calling it — the same pattern as `suppress_history`, set at the same call
  sites. `navigate` consumes it unconditionally (resetting to `Navigate`) so a
  refresh can't leave a stale "went back" primed, and stays **silent when the
  target equals the current cwd** — that covers `refresh()`, the sort/filter
  toggles and the Options → View re-navigate, none of which are a move. The
  field is seeded with `Startup` rather than `Navigate` because window creation
  navigates to the initial path: that first listing *is* the app starting, and
  playing both would have the two sounds cutting each other off every launch.

Options → Sounds stages edits in a local `Vec<String>` and only writes them on
`PSN_APPLY`, so Cancel really cancels; the only thing that happens immediately
is the preview. Preview deliberately ignores `sounds.enabled` — auditioning a
file is how you decide whether to switch sounds on. Note `CB_SETCURSEL` /
`LB_SETCURSEL` do **not** raise `CBN_SELCHANGE` / `LBN_SELCHANGE`; that is
load-bearing here, since selecting an event in the listbox programmatically
re-points the combo and a notification would re-assign and replay its sound.

### Text viewer (Alt+Enter / Alt+L)

`viewer.rs` is a singleton top-level window with a readonly multiline EDIT + Close button. Used for any "here is a block of text, copy what you need" screen — currently `op_show_properties` (Alt+Enter) and `op_dump_tree` (Alt+L). Workers compute the text off the UI thread and post `WMAPP_VIEWER_SHOW` with a `Box<(title, body)>` payload; the window proc reclaims the box and calls `viewer::show`. On open the edit takes focus and gets `EM_SETSEL(0, -1)` so Ctrl+C copies immediately.

Pure computation (folder stats, extension histogram, TOML tree dump) lives in `props.rs`, kept free of HWND / speech so the logic is unit-testable without a live window. Recursion is iterative — explicit stack, no risk of blowing the process stack on deep trees. Symlinks are counted but not followed.

**Alt+Enter branches three ways, and This PC is the one that fails silently.** A This PC row is a *display string* (`"D: (Data)"`), not a path, so `cwd.join(&entry.name)` builds `\\?\NavigatorThisPC\D: (Data)` — unstattable, so the folder walk found nothing and the screen rendered a page of zeroes. `op_show_properties` routes `cwd.is_this_pc()` to `navigator_fs::drive_info` (four constant-time volume calls: `GetVolumeInformationW`, `GetDiskFreeSpaceExW`, `GetDiskFreeSpaceW`, `GetVolumeNameForVolumeMountPointW`) → `props::format_drive_properties`. **A drive is never walked**: a recursive tally of a volume is unbounded work for a number that would then *disagree* with the capacity figures, since it can only count what the user has permission to read. The one enumeration is `props::top_level_counts`, a single non-recursive `read_dir` of the root, labelled "top level only" in the output so the counts can't be misread as a whole-disk total. An empty bay renders "Drive not ready", never zeroes — same rule as `dump_tree_toml_error`.

**Both walking screens branch on `is_remote()`, and forgetting that is silent, not loud.** `compute_folder_stats` / `dump_tree_toml` walk with `navigator_fs::read_dir` — `FindFirstFileExW` against `NavPath::as_path()`, which for a remote is the synthetic `\\?\NavigatorRemote\…` string no filesystem can enumerate. The walk fails at the root, the stack empties, and the dump renders a perfectly well-formed tree of zeroes; `read_dir_impl` even maps `ERROR_FILE_NOT_FOUND` to `Ok(vec![])`, so there may be no `errors` key to hint at it. Properties routes remotes to `rclone.stat()` + `rclone.size()` → `format_remote_properties`; Alt+L routes them to `RcloneDriver::lsjson_recursive` → `dump_tree_toml_remote`. Same class of bug as the bare-`exists()` warning under *Conflict handling*.

- **One `lsjson --recursive` for the whole tree, not a per-directory fan-out.** Each level would cost a process spawn plus a round-trip — the trap `batch.rs` exists to avoid. `--no-modtime` rides along because the dump prints paths and sizes only, and backends that keep mtime in object metadata charge a request per object for it.
- **`Path` is already root-relative and forward-slashed**, so the remote half needs no `relativize` and stays pure — `parse_lsjson_tree` is pinned against verbatim 1.73.5 output, including the `Size: -1` some backends report for directories.
- **A failed walk renders as `error = "…"`, never as an empty tree.** `dump_tree_toml_error` exists precisely so zero counts with no explanation can't come back; `render_tree_toml` is shared by all three paths so their output can't drift.
- The `root` line shows `rclone_arg()` (`mac:Downloads`), not the sentinel — same rule the title and address bar follow. `is_remotes_root()` is rejected outright, like This PC: it's a list of remotes, not a directory.

### Real Win32 dialogs

`crate::dialog::run_modal(parent, title, cx_dlu, cy_dlu, dlg_proc, init_param)` is the one way to open a modal dialog. It constructs an in-memory `DLGTEMPLATE` (`WS_POPUP | WS_CAPTION | WS_SYSMENU | DS_MODALFRAME | DS_SETFONT | DS_CENTER | DS_3DLOOK`, `WS_EX_CONTROLPARENT`), calls `DialogBoxIndirectParamW`, and the OS registers the default `#32770` dialog class — so MSAA/UIA announce `ROLE_SYSTEM_DIALOG`, Enter/Esc route through `DefDlgProc` to IDOK/IDCANCEL, and Tab traversal works without a custom pump. Controls are still built in `WM_INITDIALOG`. Do **not** roll custom `RegisterClassEx` + message pumps for dialogs — screen readers only see them as generic windows.

- Init state is passed via `Box::into_raw(Box::new(...))` → `lParam` on `WM_INITDIALOG`. The proc must `Box::from_raw` it once and stash per-dialog data in `DWLP_USER` (offset 16 on x64; we define it as a constant because the `windows` crate exports `DWL_USER` instead).
- `EndDialog(hwnd, rc)` closes a modal; never `DestroyWindow`.
- For tabbed dialogs use the **PropertySheet API** (`options.rs`), not a single dialog + `SysTabControl32` + show/hide panels. A property sheet wires each page as its own child dialog with an independent `DLGTEMPLATE` (see `dialog::build_propsheet_page_template`, flagged `WS_CHILD | DS_CONTROL`), so Ctrl+Tab cycling, tab ↔ page accessibility relationships, and per-page tab-order isolation come from the OS. Per-page commits run on `PSN_APPLY` via `DWLP_MSGRESULT = PSNRET_NOERROR`.

### History & "This PC"

- `History` (`navigator-gui/src/history.rs`) is a back/forward stack. `navigate` pushes unless `suppress_history` is set (back/forward set it before calling `navigate`).
- "This PC" is a sentinel `NavPath` (`NavPath::this_pc`, check with `is_this_pc()`). The scan worker routes it to `list_drives()` instead of `read_dir`. Navigating "up" from a drive root lands here.
- **Drive rows lead with the letter — `D: (Data)`, not Explorer's `Data (D:)`.** The row is read left to right, and the letter is what the user navigates by; putting a variable-length label first meant listening past it every time, and it made the alphabetical sort key the label rather than the letter. An unlabelled volume falls back to its kind (`E: (CD Drive)`), so the parenthetical is never empty. `drive_path_from_display` parses **only the leading token**, which is also why a real folder named `Backup (C:)` can no longer be mistaken for a volume.
- **Every volume query is wrapped in `ErrorModeGuard`** (`SetThreadErrorMode(SEM_FAILCRITICALERRORS)`). Reading the label or capacity of an empty card reader or CD bay puts up a *system modal* — "There is no disk in drive E:" — and blocks the calling thread until it's dismissed, so merely having an empty slot would stall the scan worker on every This PC listing. Thread-scoped on purpose: `SetErrorMode` is process-wide and would change behaviour for the UI thread and every plugin too.

### Remote browsing (rclone)

- **Two virtual sentinels, plus a prefix.** `NavPath::remotes_root()` (`\\?\NavigatorRemotes`, check `is_remotes_root()`) is the listing of every configured rclone remote. Individual remote paths store as `\\?\NavigatorRemote\<name>\<sub\with\backslashes>` (prefix constant `REMOTE_PREFIX`, check `is_remote()` / `is_remote_root()`). Tools → Connect to remote… navigates to `remotes_root`; activating an entry from there calls `NavPath::remotes_root().join("<name>")` which is special-cased in `join` to build a remote root instead of concatenating onto the sentinel.
- **Scan worker branches three ways** before falling through to `read_dir`: `is_this_pc` → `list_drives`, `is_remotes_root` → `RcloneDriver::listremotes`, `is_remote` → `RcloneDriver::lsjson(name, sub)`. The worker receives its own `RcloneDriver` clone at startup (`scan_rclone` in `AppState::new`) — don't try to share the app's main driver, they have independent lifetimes.
- **`rclone lsjson` parsing.** `navigator-rclone` maps rclone's JSON (`Name` / `Size` / `IsDir` / `ModTime`) to `Entry`, converting RFC3339 timestamps to `FileTime` via a Howard-Hinnant `days_from_civil` helper. `attrs` / `hidden` / `system` / `created` stay zero — rclone doesn't surface Windows attributes. Second-granularity is fine; the UI never renders sub-second.
- **CLI arg translation.** `Operation` variants still carry `NavPath`; `push_op_args` now routes through `nav_arg(p)` which returns `p.rclone_arg()` (`remote:sub/path`) when remote, else falls through to the existing `path_arg` for local paths. `copyto` / `moveto` / `purge` are the same verbs regardless of endpoint, so remote↔local / remote↔remote work with no extra code.
- **Address bar accepts rclone syntax.** `NavPath::new("gdrive:")` / `NavPath::new("mac:Downloads/incoming")` build remote paths directly when the prefix-before-colon is a valid rclone name (more than one char, alphanumeric/underscore/dash/dot). Bare one-letter prefixes are treated as Windows drive-relative paths and still rejected. Title + address bar display uses `rclone_arg()` so the user sees `mac:Downloads` rather than `\\?\NavigatorRemote\mac\Downloads`.
- **CLI.** `navigator -r <remote>[:sub]` / `navigator --remote <remote>[:sub]` opens a remote at launch. A bare `-r mac` becomes `mac:` (remote root). Passing `mac:Downloads` without `-r` also works because `NavPath::new` accepts the syntax; `-r` exists for discoverability and to let users pass a bare remote name. `parse_args` in `crates/navigator/src/main.rs` has unit tests covering every form.
- **Watcher skipped.** `AppState::watch_cwd` drops any existing watcher for `is_remotes_root` / `is_remote` paths — `notify` can't watch anything behind rclone, and reusing a stale local watcher would be worse than nothing.
- **rclone config.** rclone auto-discovers `%APPDATA%\rclone\rclone.conf`; we don't pass `--config`. If the user reconfigures remotes while the app is running, a second "Connect to remote…" call re-invokes `listremotes` and picks up the new set.
- **Opening remote files → stage → watch → upload prompt.** `AppState::open_file` checks `path.is_remote()` and routes to `open_remote_file`, which spawns a worker that runs `Operation::CopyTo { src: remote, dst: <exe_dir>/.remote-cache/<remote>/<sub>/<file> }` and then `ShellExecute`s the staged copy. The staging machinery lives in `navigator-gui/src/remote_cache.rs` (`RemoteCache` struct on `AppState::remote_cache`). Each staged file records `(remote NavPath, last_known_mtime, prompting flag)`. A single process-wide `notify::RecommendedWatcher` is lazily armed on the cache root (`ensure_watcher`, recursive) the first time a file is registered — posts `WMAPP_REMOTE_EDIT` with `Box<PathBuf>` payload when a recorded path's mtime advances. The window proc reclaims the box, calls `prompt_remote_upload` (Yes/No `MessageBoxW`), and on Yes invokes `AppState::op_remote_upload` which spawns another `CopyTo` worker to push the staged copy back. `RemoteCache::finish_prompt(path, new_mtime)` clears the `prompting` flag and re-baselines the mtime so the next save re-triggers rather than fires forever. Stage files are never auto-purged — same no-cleanup stance as per-volume `.trash/`.

### Config & persistence

- `ConfigHandle::load_or_default()` is infallible — a corrupt `config.toml` logs a warning and returns defaults.
- `config.toml`, `plugins/`, `clipboard.json`, `clipboard_history.json`, and `.trash/` all live next to the exe (or in the case of `.trash`, at each volume root). `navigator_config::exe_dir()` is the source of truth; don't hardcode.
- Sort mode, filters (show hidden/system), shortcut bindings, hotspot slots, and per-column visibility (`general.columns`) all persist here.
- **`[general] announce_interval_secs`** (default 5) is the spoken-progress cadence — see *Progress reporting* above. Options → Speech writes it.
- **`[rclone]` section** holds `progress_window` (moved out of `[general]`), `transfers` (default 8, clamped 1..=64 via `Rclone::transfers_clamped`), and `on_conflict` (default `"update"`). Options → Rclone tab writes all three. `transfers` feeds rclone's `--transfers N`. The `on_conflict` combo deliberately omits `Mirror` — a hand-edited config that sets it is honoured, and opening Options won't silently rewrite it (the commit only writes the mode when the combo has a real selection).
- **`[sounds]` section** holds `enabled` (default true — a master mute that keeps assignments) and the `[sounds.events]` table. See *Event sounds* above. `navigator_sounds/` sits next to the exe like everything else; it is created on demand by Options → Sounds, never at startup.
- **`[extraction]` section** holds `delete_when_extracted` (default true) and `create_folder` (default true). Options → Extraction tab writes both. Read by `AppState::op_extract` per invocation so a config save applies to the next extract without a restart.
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

See `README.md` for the user-facing table. User-bound actions live under `shortcuts` in `config.toml`; `navigator_config::shortcuts::default_actions()` returns the seeded defaults (Copy/Cut/Paste/PasteSpecial[Ctrl+Shift+V]/Append/CopyPaths/SelectAll/Rename/Refresh/ToggleHidden/ToggleSystem/Search/NavigateUp/Hist Back+Forward/Undo + Hotspot1..10 + HotspotSet1..10 + ShowProperties[Alt+Enter] + DumpTree[Alt+L] + NewFolder[Ctrl+N] + NewFile[Ctrl+Shift+N]). The accel table is rebuilt on startup and on shortcut-editor save via `window::rebuild_accels`. `default_chords_are_unique` in `navigator-config/tests/config.rs` guards against a new default silently shadowing an existing chord — the accel table matches modifiers strictly, so a collision means one action never fires.

The `new_folder.rs` dialog serves both `NewFolder` (Ctrl+N) and `NewFile` (Ctrl+Shift+N) via a `Kind` enum — `open` / `open_file` are the two entry points. `op_new_file` requires a non-empty segment after the final dot so ShellExecute can resolve a handler — `name.contains('.') && !name.ends_with('.')`, which accepts `notes.txt` and dotfiles like `.gitignore` but rejects `notes` / `notes.`. It creates the file with `Operation::Touch` (`rclone touch`, works local + remote), then opens it via `open_file`. A pre-existing local file is opened rather than clobbered.

Adding a new `InternalCommand` variant touches three places: enum in `shortcuts.rs`, seed line in `default_actions()`, and a `dispatch_internal` arm in `window.rs`. Accel matches modifiers strictly, so `Alt+Enter` does **not** collide with the listview's plain-Enter handler (those are distinct ACCEL entries only when modifiers match).

A user's existing `config.toml` does NOT auto-pick up new default shortcuts — `default_actions()` only seeds on first run. Ship the breaking default change, expect the user to either re-run with no config or hand-edit. There is intentionally no migration chain.
