//! Shortcut/Actions editor — a modal list dialog with add/edit/remove,
//! plus a nested modal for editing a single [`ShortcutAction`].
//!
//! Both dialogs are built via [`crate::dialog::run_modal`] so they're
//! real `#32770` dialog windows; screen readers announce them as
//! dialogs and keyboard traversal runs through `DefDlgProc`.

use std::ffi::c_void;
use std::iter::once;
use std::sync::Arc;

use parking_lot::Mutex;

use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::Graphics::Gdi::{DEFAULT_GUI_FONT, GetStockObject};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    BS_AUTOCHECKBOX, BS_PUSHBUTTON, CreateWindowExW, EndDialog, GetWindowLongPtrW,
    GetWindowTextLengthW, GetWindowTextW, HMENU, SendMessageW, SetWindowLongPtrW, SetWindowTextW,
    WINDOW_EX_STYLE, WINDOW_LONG_PTR_INDEX, WM_COMMAND, WM_INITDIALOG, WM_SETFONT, WS_BORDER,
    WS_CHILD, WS_TABSTOP, WS_VISIBLE,
};
use windows::core::{PCWSTR, w};

// DWLP_USER offset on a dialog window (x64: 16). See `options.rs` for notes.
const DWLP_USER: WINDOW_LONG_PTR_INDEX = WINDOW_LONG_PTR_INDEX(16);

use navigator_config::{InternalCommand, ShortcutAction, ShortcutChord};

use crate::app::AppState;

const ID_LB: u16 = 400;
const ID_BTN_ADD: u16 = 401;
const ID_BTN_EDIT: u16 = 402;
const ID_BTN_REMOVE: u16 = 403;
const ID_BTN_OK: u16 = 1;
const ID_BTN_CANCEL: u16 = 2;

// Edit sub-dialog
const ID_E_NAME: u16 = 500;
const ID_E_CTRL: u16 = 501;
const ID_E_SHIFT: u16 = 502;
const ID_E_ALT: u16 = 503;
const ID_E_KEY: u16 = 504;
const ID_E_CMD: u16 = 505;
const ID_E_ARGS: u16 = 506;
const ID_E_SINGLE: u16 = 507;
const ID_E_PRESET: u16 = 508;

/// Predefined templates. First entry is "Custom" — picking it doesn't
/// touch the form, so the user can edit freely. Entries with
/// `internal = Some(..)` seed a built-in UI command binding; entries
/// with `internal = None` seed an external program launch.
struct Preset {
    label: &'static str,
    name: &'static str,
    internal: Option<InternalCommand>,
    command: &'static str,
    args: &'static [&'static str],
    single: bool,
}

const PRESETS: &[Preset] = &[
    Preset {
        label: "— Custom —",
        name: "",
        internal: None,
        command: "",
        args: &[],
        single: false,
    },
    // Built-in UI commands.
    Preset {
        label: "Built-in: Copy",
        name: "Copy",
        internal: Some(InternalCommand::Copy),
        command: "",
        args: &[],
        single: false,
    },
    Preset {
        label: "Built-in: Cut",
        name: "Cut",
        internal: Some(InternalCommand::Cut),
        command: "",
        args: &[],
        single: false,
    },
    Preset {
        label: "Built-in: Append to copy",
        name: "Append to copy",
        internal: Some(InternalCommand::AppendCopy),
        command: "",
        args: &[],
        single: false,
    },
    Preset {
        label: "Built-in: Append to cut",
        name: "Append to cut",
        internal: Some(InternalCommand::AppendCut),
        command: "",
        args: &[],
        single: false,
    },
    Preset {
        label: "Built-in: Paste",
        name: "Paste",
        internal: Some(InternalCommand::Paste),
        command: "",
        args: &[],
        single: false,
    },
    Preset {
        label: "Built-in: Copy paths",
        name: "Copy paths",
        internal: Some(InternalCommand::CopyPaths),
        command: "",
        args: &[],
        single: false,
    },
    Preset {
        label: "Built-in: Delete",
        name: "Delete",
        internal: Some(InternalCommand::Delete),
        command: "",
        args: &[],
        single: false,
    },
    Preset {
        label: "Built-in: Rename",
        name: "Rename",
        internal: Some(InternalCommand::Rename),
        command: "",
        args: &[],
        single: false,
    },
    Preset {
        label: "Built-in: Select all",
        name: "Select all",
        internal: Some(InternalCommand::SelectAll),
        command: "",
        args: &[],
        single: false,
    },
    Preset {
        label: "Built-in: Refresh",
        name: "Refresh",
        internal: Some(InternalCommand::Refresh),
        command: "",
        args: &[],
        single: false,
    },
    Preset {
        label: "Built-in: Toggle hidden",
        name: "Toggle hidden",
        internal: Some(InternalCommand::ToggleHidden),
        command: "",
        args: &[],
        single: false,
    },
    Preset {
        label: "Built-in: Toggle system",
        name: "Toggle system",
        internal: Some(InternalCommand::ToggleSystem),
        command: "",
        args: &[],
        single: false,
    },
    Preset {
        label: "Built-in: Find in folder",
        name: "Find in folder",
        internal: Some(InternalCommand::Search),
        command: "",
        args: &[],
        single: false,
    },
    Preset {
        label: "Built-in: Navigate up",
        name: "Navigate up",
        internal: Some(InternalCommand::NavigateUp),
        command: "",
        args: &[],
        single: false,
    },
    Preset {
        label: "Built-in: History back",
        name: "History back",
        internal: Some(InternalCommand::HistBack),
        command: "",
        args: &[],
        single: false,
    },
    Preset {
        label: "Built-in: History forward",
        name: "History forward",
        internal: Some(InternalCommand::HistForward),
        command: "",
        args: &[],
        single: false,
    },
    Preset {
        label: "Built-in: Undo",
        name: "Undo",
        internal: Some(InternalCommand::Undo),
        command: "",
        args: &[],
        single: false,
    },
    Preset {
        label: "Built-in: Hotspot 1 (goto)",
        name: "Hotspot 1",
        internal: Some(InternalCommand::Hotspot1),
        command: "",
        args: &[],
        single: false,
    },
    Preset {
        label: "Built-in: Hotspot 2 (goto)",
        name: "Hotspot 2",
        internal: Some(InternalCommand::Hotspot2),
        command: "",
        args: &[],
        single: false,
    },
    Preset {
        label: "Built-in: Hotspot 3 (goto)",
        name: "Hotspot 3",
        internal: Some(InternalCommand::Hotspot3),
        command: "",
        args: &[],
        single: false,
    },
    Preset {
        label: "Built-in: Hotspot 4 (goto)",
        name: "Hotspot 4",
        internal: Some(InternalCommand::Hotspot4),
        command: "",
        args: &[],
        single: false,
    },
    Preset {
        label: "Built-in: Hotspot 5 (goto)",
        name: "Hotspot 5",
        internal: Some(InternalCommand::Hotspot5),
        command: "",
        args: &[],
        single: false,
    },
    Preset {
        label: "Built-in: Hotspot 6 (goto)",
        name: "Hotspot 6",
        internal: Some(InternalCommand::Hotspot6),
        command: "",
        args: &[],
        single: false,
    },
    Preset {
        label: "Built-in: Hotspot 7 (goto)",
        name: "Hotspot 7",
        internal: Some(InternalCommand::Hotspot7),
        command: "",
        args: &[],
        single: false,
    },
    Preset {
        label: "Built-in: Hotspot 8 (goto)",
        name: "Hotspot 8",
        internal: Some(InternalCommand::Hotspot8),
        command: "",
        args: &[],
        single: false,
    },
    Preset {
        label: "Built-in: Hotspot 9 (goto)",
        name: "Hotspot 9",
        internal: Some(InternalCommand::Hotspot9),
        command: "",
        args: &[],
        single: false,
    },
    Preset {
        label: "Built-in: Hotspot 10 (goto)",
        name: "Hotspot 10",
        internal: Some(InternalCommand::Hotspot10),
        command: "",
        args: &[],
        single: false,
    },
    Preset {
        label: "Built-in: Set hotspot 1",
        name: "Set hotspot 1",
        internal: Some(InternalCommand::HotspotSet1),
        command: "",
        args: &[],
        single: false,
    },
    Preset {
        label: "Built-in: Set hotspot 2",
        name: "Set hotspot 2",
        internal: Some(InternalCommand::HotspotSet2),
        command: "",
        args: &[],
        single: false,
    },
    Preset {
        label: "Built-in: Set hotspot 3",
        name: "Set hotspot 3",
        internal: Some(InternalCommand::HotspotSet3),
        command: "",
        args: &[],
        single: false,
    },
    Preset {
        label: "Built-in: Set hotspot 4",
        name: "Set hotspot 4",
        internal: Some(InternalCommand::HotspotSet4),
        command: "",
        args: &[],
        single: false,
    },
    Preset {
        label: "Built-in: Set hotspot 5",
        name: "Set hotspot 5",
        internal: Some(InternalCommand::HotspotSet5),
        command: "",
        args: &[],
        single: false,
    },
    Preset {
        label: "Built-in: Set hotspot 6",
        name: "Set hotspot 6",
        internal: Some(InternalCommand::HotspotSet6),
        command: "",
        args: &[],
        single: false,
    },
    Preset {
        label: "Built-in: Set hotspot 7",
        name: "Set hotspot 7",
        internal: Some(InternalCommand::HotspotSet7),
        command: "",
        args: &[],
        single: false,
    },
    Preset {
        label: "Built-in: Set hotspot 8",
        name: "Set hotspot 8",
        internal: Some(InternalCommand::HotspotSet8),
        command: "",
        args: &[],
        single: false,
    },
    Preset {
        label: "Built-in: Set hotspot 9",
        name: "Set hotspot 9",
        internal: Some(InternalCommand::HotspotSet9),
        command: "",
        args: &[],
        single: false,
    },
    Preset {
        label: "Built-in: Set hotspot 10",
        name: "Set hotspot 10",
        internal: Some(InternalCommand::HotspotSet10),
        command: "",
        args: &[],
        single: false,
    },
    // External launch templates.
    Preset {
        label: "Open in Terminal",
        name: "Open in Terminal",
        internal: None,
        command: "wt.exe",
        args: &["-d", "{folder}"],
        single: true,
    },
    Preset {
        label: "Open in VSCode",
        name: "Open in VSCode",
        internal: None,
        command: "code",
        args: &["{path}"],
        single: false,
    },
    Preset {
        label: "Open in Explorer (reveal)",
        name: "Open in Explorer",
        internal: None,
        command: "explorer.exe",
        args: &["/select,{path}"],
        single: true,
    },
    Preset {
        label: "Open in Notepad",
        name: "Open in Notepad",
        internal: None,
        command: "notepad.exe",
        args: &["{path}"],
        single: false,
    },
    Preset {
        label: "Open PowerShell here",
        name: "PowerShell here",
        internal: None,
        command: "powershell.exe",
        args: &[
            "-NoExit",
            "-Command",
            "Set-Location -LiteralPath '{folder}'",
        ],
        single: true,
    },
];

struct ListData {
    state: Arc<AppState>,
    lb: HWND,
    actions: Vec<ShortcutAction>,
    dirty: bool,
}

struct EditInit {
    start: ShortcutAction,
    result: Arc<Mutex<Option<ShortcutAction>>>,
}

/// Open the shortcut editor as a modal.
pub fn open(parent: HWND, state: Arc<AppState>) -> windows::core::Result<()> {
    let boxed = Box::into_raw(Box::new(state)) as isize;
    crate::dialog::run_modal(
        parent,
        "Shortcuts & Actions — navigator",
        350,
        300,
        Some(list_dialog_proc),
        boxed,
    );
    Ok(())
}

unsafe extern "system" fn list_dialog_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> isize {
    match msg {
        WM_INITDIALOG => unsafe {
            let state_box = Box::from_raw(lp.0 as *mut Arc<AppState>);
            let state: Arc<AppState> = *state_box;

            let font = GetStockObject(DEFAULT_GUI_FONT);
            let apply_font = |h: HWND| {
                SendMessageW(
                    h,
                    WM_SETFONT,
                    Some(WPARAM(font.0 as usize)),
                    Some(LPARAM(1)),
                );
            };
            let label = mkstatic(hwnd, "Configured shortcuts:", 10, 10, 300);
            apply_font(label);
            let lb = mklistbox(hwnd, 10, 32, 540, 330, ID_LB);
            apply_font(lb);

            let btn_add = mkbutton(hwnd, "&Add…", 10, 370, 90, 26, ID_BTN_ADD);
            let btn_edit = mkbutton(hwnd, "&Edit…", 110, 370, 90, 26, ID_BTN_EDIT);
            let btn_remove = mkbutton(hwnd, "&Remove", 210, 370, 90, 26, ID_BTN_REMOVE);
            apply_font(btn_add);
            apply_font(btn_edit);
            apply_font(btn_remove);

            let btn_ok = mkbutton(hwnd, "OK", 380, 410, 80, 28, ID_BTN_OK);
            let btn_cancel = mkbutton(hwnd, "Cancel", 470, 410, 80, 28, ID_BTN_CANCEL);
            apply_font(btn_ok);
            apply_font(btn_cancel);

            let actions = state.actions();
            let data = Box::new(ListData {
                state,
                lb,
                actions,
                dirty: false,
            });
            let raw = Box::into_raw(data);
            SetWindowLongPtrW(hwnd, DWLP_USER, raw as isize);
            refill(&*raw);
            1
        },
        WM_COMMAND => unsafe {
            let cmd = (wp.0 & 0xFFFF) as u16;
            let Some(d) = list_data(hwnd) else {
                return 0;
            };
            match cmd {
                ID_BTN_ADD => {
                    if let Some(new) = edit_action(
                        hwnd,
                        ShortcutAction {
                            name: "New action".into(),
                            chord: ShortcutChord::default(),
                            internal: None,
                            command: String::new(),
                            args: Vec::new(),
                            single: false,
                        },
                    ) {
                        d.actions.push(new);
                        d.dirty = true;
                        refill(d);
                    }
                    1
                }
                ID_BTN_EDIT => {
                    let idx = lb_selected(d.lb);
                    if let Some(i) = idx {
                        let current = d.actions[i].clone();
                        if let Some(updated) = edit_action(hwnd, current) {
                            d.actions[i] = updated;
                            d.dirty = true;
                            refill(d);
                        }
                    }
                    1
                }
                ID_BTN_REMOVE => {
                    let idx = lb_selected(d.lb);
                    if let Some(i) = idx {
                        d.actions.remove(i);
                        d.dirty = true;
                        refill(d);
                    }
                    1
                }
                ID_BTN_OK => {
                    if d.dirty {
                        d.state.config.with_mut(|c| c.shortcuts = d.actions.clone());
                        let _ = d.state.config.save();
                        if let Some(h) = d.state.main_hwnd() {
                            crate::window::rebuild_accels(h);
                        }
                        d.state.say("shortcuts saved", false);
                    }
                    let raw = GetWindowLongPtrW(hwnd, DWLP_USER);
                    if raw != 0 {
                        let _ = Box::from_raw(raw as *mut ListData);
                        SetWindowLongPtrW(hwnd, DWLP_USER, 0);
                    }
                    let _ = EndDialog(hwnd, cmd as isize);
                    1
                }
                ID_BTN_CANCEL => {
                    let raw = GetWindowLongPtrW(hwnd, DWLP_USER);
                    if raw != 0 {
                        let _ = Box::from_raw(raw as *mut ListData);
                        SetWindowLongPtrW(hwnd, DWLP_USER, 0);
                    }
                    let _ = EndDialog(hwnd, cmd as isize);
                    1
                }
                _ => 0,
            }
        },
        _ => 0,
    }
}

unsafe fn list_data<'a>(hwnd: HWND) -> Option<&'a mut ListData> {
    let raw = unsafe { GetWindowLongPtrW(hwnd, DWLP_USER) };
    if raw == 0 {
        None
    } else {
        Some(unsafe { &mut *(raw as *mut ListData) })
    }
}

fn refill(d: &ListData) {
    // LB_RESETCONTENT 0x0184, LB_ADDSTRING 0x0180
    unsafe {
        SendMessageW(d.lb, 0x0184, Some(WPARAM(0)), Some(LPARAM(0)));
    }
    for a in &d.actions {
        let target: String = match a.internal {
            Some(ic) => format!("built-in {:?}", ic),
            None if a.command.is_empty() => "(no command)".into(),
            None => a.command.clone(),
        };
        let line = format!("{} — {} → {}", format_chord(&a.chord), a.name, target);
        let wz: Vec<u16> = line.encode_utf16().chain([0]).collect();
        unsafe {
            SendMessageW(
                d.lb,
                0x0180,
                Some(WPARAM(0)),
                Some(LPARAM(wz.as_ptr() as isize)),
            );
        }
    }
}

fn format_chord(c: &ShortcutChord) -> String {
    let mut out = String::new();
    if c.ctrl {
        out.push_str("Ctrl+");
    }
    if c.shift {
        out.push_str("Shift+");
    }
    if c.alt {
        out.push_str("Alt+");
    }
    if c.key.is_empty() {
        out.push_str("(unset)");
    } else {
        out.push_str(&c.key);
    }
    out
}

fn lb_selected(lb: HWND) -> Option<usize> {
    // LB_GETCURSEL 0x0188
    let rc = unsafe { SendMessageW(lb, 0x0188, Some(WPARAM(0)), Some(LPARAM(0))).0 };
    if rc < 0 { None } else { Some(rc as usize) }
}

// ---- editor sub-dialog ---------------------------------------------------

struct EditData {
    preset: HWND,
    name: HWND,
    ctrl: HWND,
    shift: HWND,
    alt: HWND,
    key: HWND,
    cmd: HWND,
    args: HWND,
    single: HWND,
    kind_label: HWND,
    /// Latest `InternalCommand` picked by a preset, or carried over from
    /// the action being edited. `None` = external launch.
    internal: Arc<Mutex<Option<InternalCommand>>>,
    result: Arc<Mutex<Option<ShortcutAction>>>,
}

fn edit_action(parent: HWND, start: ShortcutAction) -> Option<ShortcutAction> {
    let result = Arc::new(Mutex::new(None::<ShortcutAction>));
    let init = Box::into_raw(Box::new(EditInit {
        start,
        result: result.clone(),
    })) as isize;
    crate::dialog::run_modal(
        parent,
        "Edit action — navigator",
        330,
        330,
        Some(edit_dialog_proc),
        init,
    );
    let g = result.lock();
    g.clone()
}

unsafe extern "system" fn edit_dialog_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> isize {
    match msg {
        WM_INITDIALOG => unsafe {
            let init_box = Box::from_raw(lp.0 as *mut EditInit);
            let init: EditInit = *init_box;
            let font = GetStockObject(DEFAULT_GUI_FONT);
            let apply_font = |h: HWND| {
                SendMessageW(
                    h,
                    WM_SETFONT,
                    Some(WPARAM(font.0 as usize)),
                    Some(LPARAM(1)),
                );
            };

            // Preset picker — fills the form with a common template. Kept
            // first so screen-reader users encounter it before the fields
            // they'd otherwise have to fill by hand.
            apply_font(mkstatic(hwnd, "Preset:", 10, 12, 80));
            let preset = mkcombo(hwnd, 100, 10, 400, 200, ID_E_PRESET);
            apply_font(preset);
            for p in PRESETS {
                let wz: Vec<u16> = p.label.encode_utf16().chain(once(0)).collect();
                // CB_ADDSTRING = 0x0143
                SendMessageW(
                    preset,
                    0x0143,
                    Some(WPARAM(0)),
                    Some(LPARAM(wz.as_ptr() as isize)),
                );
            }
            // CB_SETCURSEL = 0x014E — default to "Custom".
            SendMessageW(preset, 0x014E, Some(WPARAM(0)), Some(LPARAM(0)));

            // Name
            apply_font(mkstatic(hwnd, "Name:", 10, 46, 80));
            let name = mkedit(hwnd, 100, 44, 400, ID_E_NAME);
            apply_font(name);

            // Chord
            apply_font(mkstatic(hwnd, "Modifiers:", 10, 80, 80));
            let ctrl = mkcheck(hwnd, "&Ctrl", 100, 80, ID_E_CTRL);
            apply_font(ctrl);
            let shift = mkcheck(hwnd, "&Shift", 170, 80, ID_E_SHIFT);
            apply_font(shift);
            let alt = mkcheck(hwnd, "&Alt", 240, 80, ID_E_ALT);
            apply_font(alt);

            apply_font(mkstatic(hwnd, "Key:", 10, 114, 80));
            let key = mkedit(hwnd, 100, 112, 120, ID_E_KEY);
            apply_font(key);
            apply_font(mkstatic(
                hwnd,
                "(A-Z, 0-9, F1..F24, Up/Down/Left/Right, Home/End/PageUp/PageDown, Insert/Delete/Tab/Space/Escape/Enter/Backspace)",
                230,
                114,
                250,
            ));

            // Command
            apply_font(mkstatic(hwnd, "Command:", 10, 148, 80));
            let cmd = mkedit(hwnd, 100, 146, 400, ID_E_CMD);
            apply_font(cmd);

            // Args (multiline)
            apply_font(mkstatic(hwnd, "Arguments (one per line):", 10, 180, 280));
            apply_font(mkstatic(
                hwnd,
                "Placeholders: {path}, {folder}, {parent}, {name}",
                10,
                344,
                440,
            ));
            let args = mkmultiedit(hwnd, 10, 202, 490, 140, ID_E_ARGS);
            apply_font(args);

            let single = mkcheck(
                hwnd,
                "Run once even when multiple items selected",
                10,
                370,
                ID_E_SINGLE,
            );
            apply_font(single);

            // Read-only label showing whether this action runs a built-in
            // command or launches a program. Updated when a preset is
            // picked; driven by the internal Mutex state.
            let kind_label = mkstatic(hwnd, "Kind: (choose a preset)", 10, 400, 490);
            apply_font(kind_label);

            let btn_ok = mkbutton(hwnd, "OK", 320, 440, 80, 28, ID_BTN_OK);
            apply_font(btn_ok);
            let btn_cancel = mkbutton(hwnd, "Cancel", 410, 440, 80, 28, ID_BTN_CANCEL);
            apply_font(btn_cancel);

            // Seed controls with current values.
            set_text(name, &init.start.name);
            set_check(ctrl, init.start.chord.ctrl);
            set_check(shift, init.start.chord.shift);
            set_check(alt, init.start.chord.alt);
            set_text(key, &init.start.chord.key);
            set_text(cmd, &init.start.command);
            set_text(args, &init.start.args.join("\r\n"));
            set_check(single, init.start.single);

            let internal = Arc::new(Mutex::new(init.start.internal));
            update_kind_label(kind_label, &internal.lock(), &init.start.command);

            let data = Box::new(EditData {
                preset,
                name,
                ctrl,
                shift,
                alt,
                key,
                cmd,
                args,
                single,
                kind_label,
                internal,
                result: init.result,
            });
            SetWindowLongPtrW(hwnd, DWLP_USER, Box::into_raw(data) as isize);
            1
        },
        WM_COMMAND => unsafe {
            let cmd = (wp.0 & 0xFFFF) as u16;
            let notif = ((wp.0 >> 16) & 0xFFFF) as u32;
            let Some(d) = edit_data(hwnd) else {
                return 0;
            };
            // CBN_SELCHANGE = 1
            if cmd == ID_E_PRESET && notif == 1 {
                apply_preset(d);
                return 1;
            }
            match cmd {
                ID_BTN_OK => {
                    let internal = *d.internal.lock();
                    let action = ShortcutAction {
                        name: get_text(d.name),
                        chord: ShortcutChord {
                            ctrl: get_check(d.ctrl),
                            shift: get_check(d.shift),
                            alt: get_check(d.alt),
                            key: get_text(d.key).trim().to_string(),
                        },
                        internal,
                        command: get_text(d.cmd),
                        args: get_text(d.args).lines().map(|s| s.to_string()).collect(),
                        single: get_check(d.single),
                    };
                    *d.result.lock() = Some(action);
                    let raw = GetWindowLongPtrW(hwnd, DWLP_USER);
                    if raw != 0 {
                        let _ = Box::from_raw(raw as *mut EditData);
                        SetWindowLongPtrW(hwnd, DWLP_USER, 0);
                    }
                    let _ = EndDialog(hwnd, cmd as isize);
                    return 1;
                }
                ID_BTN_CANCEL => {
                    let raw = GetWindowLongPtrW(hwnd, DWLP_USER);
                    if raw != 0 {
                        let _ = Box::from_raw(raw as *mut EditData);
                        SetWindowLongPtrW(hwnd, DWLP_USER, 0);
                    }
                    let _ = EndDialog(hwnd, cmd as isize);
                    return 1;
                }
                _ => 0,
            }
        },
        _ => 0,
    }
}

unsafe fn edit_data<'a>(hwnd: HWND) -> Option<&'a mut EditData> {
    let raw = unsafe { GetWindowLongPtrW(hwnd, DWLP_USER) };
    if raw == 0 {
        None
    } else {
        Some(unsafe { &mut *(raw as *mut EditData) })
    }
}

/// Copy the current preset's values into the form. Skips the "Custom"
/// slot (index 0) so the user's in-progress edits aren't wiped when they
/// reopen the combobox.
fn apply_preset(d: &EditData) {
    // CB_GETCURSEL = 0x0147
    let idx = unsafe { SendMessageW(d.preset, 0x0147, Some(WPARAM(0)), Some(LPARAM(0))).0 };
    if idx <= 0 {
        return;
    }
    let Some(p) = PRESETS.get(idx as usize) else {
        return;
    };
    set_text(d.name, p.name);
    set_text(d.cmd, p.command);
    set_text(d.args, &p.args.join("\r\n"));
    set_check(d.single, p.single);
    *d.internal.lock() = p.internal;
    update_kind_label(d.kind_label, &d.internal.lock(), p.command);
}

/// Reflect the current kind (built-in vs launch) in the read-only label
/// under the form. Called on preset apply + on initial seed.
fn update_kind_label(h: HWND, internal: &Option<InternalCommand>, cmd: &str) {
    let text = match internal {
        Some(ic) => format!("Kind: built-in ({:?}) — command/args ignored", ic),
        None if cmd.is_empty() => "Kind: launch program (no command set)".to_string(),
        None => format!("Kind: launch program ({})", cmd),
    };
    set_text(h, &text);
}

// --- control builders (slim; share with options.rs some day) ---

fn mkstatic(parent: HWND, text: &str, x: i32, y: i32, w: i32) -> HWND {
    let t: Vec<u16> = text.encode_utf16().chain(once(0)).collect();
    unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("STATIC"),
            PCWSTR(t.as_ptr()),
            windows::Win32::UI::WindowsAndMessaging::WINDOW_STYLE(WS_CHILD.0 | WS_VISIBLE.0),
            x,
            y,
            w,
            20,
            Some(parent),
            None,
            Some(GetModuleHandleW(None).unwrap().into()),
            None,
        )
        .unwrap()
    }
}

fn mkedit(parent: HWND, x: i32, y: i32, w: i32, id: u16) -> HWND {
    unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("EDIT"),
            w!(""),
            windows::Win32::UI::WindowsAndMessaging::WINDOW_STYLE(
                WS_CHILD.0 | WS_VISIBLE.0 | WS_BORDER.0 | WS_TABSTOP.0,
            ),
            x,
            y,
            w,
            22,
            Some(parent),
            Some(HMENU(id as isize as *mut c_void)),
            Some(GetModuleHandleW(None).unwrap().into()),
            None,
        )
        .unwrap()
    }
}

fn mkmultiedit(parent: HWND, x: i32, y: i32, w: i32, h: i32, id: u16) -> HWND {
    let style = WS_CHILD.0 | WS_VISIBLE.0 | WS_BORDER.0 | WS_TABSTOP.0
        | 0x00200000 /* WS_VSCROLL */
        | 0x0004     /* ES_MULTILINE */
        | 0x0040     /* ES_AUTOVSCROLL */
        | 0x0100; /* ES_WANTRETURN */
    unsafe {
        let h = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("EDIT"),
            w!(""),
            windows::Win32::UI::WindowsAndMessaging::WINDOW_STYLE(style),
            x,
            y,
            w,
            h,
            Some(parent),
            Some(HMENU(id as isize as *mut c_void)),
            Some(GetModuleHandleW(None).unwrap().into()),
            None,
        )
        .unwrap();
        // Multiline edits capture Tab for indentation by default. We need
        // Tab to leave the control, so drive focus traversal ourselves.
        crate::window::install_tab_nav(h);
        h
    }
}

fn mkcheck(parent: HWND, text: &str, x: i32, y: i32, id: u16) -> HWND {
    let t: Vec<u16> = text.encode_utf16().chain(once(0)).collect();
    unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("BUTTON"),
            PCWSTR(t.as_ptr()),
            windows::Win32::UI::WindowsAndMessaging::WINDOW_STYLE(
                WS_CHILD.0 | WS_VISIBLE.0 | WS_TABSTOP.0 | BS_AUTOCHECKBOX as u32,
            ),
            x,
            y,
            120,
            22,
            Some(parent),
            Some(HMENU(id as isize as *mut c_void)),
            Some(GetModuleHandleW(None).unwrap().into()),
            None,
        )
        .unwrap()
    }
}

fn mkbutton(parent: HWND, text: &str, x: i32, y: i32, w: i32, h: i32, id: u16) -> HWND {
    let t: Vec<u16> = text.encode_utf16().chain(once(0)).collect();
    unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("BUTTON"),
            PCWSTR(t.as_ptr()),
            windows::Win32::UI::WindowsAndMessaging::WINDOW_STYLE(
                WS_CHILD.0 | WS_VISIBLE.0 | WS_TABSTOP.0 | BS_PUSHBUTTON as u32,
            ),
            x,
            y,
            w,
            h,
            Some(parent),
            Some(HMENU(id as isize as *mut c_void)),
            Some(GetModuleHandleW(None).unwrap().into()),
            None,
        )
        .unwrap()
    }
}

fn mkcombo(parent: HWND, x: i32, y: i32, w: i32, h: i32, id: u16) -> HWND {
    // CBS_DROPDOWNLIST = 0x0003, CBS_HASSTRINGS = 0x0200, WS_VSCROLL = 0x00200000.
    // The passed h includes the dropped-down area; the visible closed
    // height is driven by the font.
    let style = WS_CHILD.0 | WS_VISIBLE.0 | WS_TABSTOP.0 | 0x00200000 | 0x0003 | 0x0200;
    unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("COMBOBOX"),
            w!(""),
            windows::Win32::UI::WindowsAndMessaging::WINDOW_STYLE(style),
            x,
            y,
            w,
            h,
            Some(parent),
            Some(HMENU(id as isize as *mut c_void)),
            Some(GetModuleHandleW(None).unwrap().into()),
            None,
        )
        .unwrap()
    }
}

fn mklistbox(parent: HWND, x: i32, y: i32, w: i32, h: i32, id: u16) -> HWND {
    let style = WS_CHILD.0 | WS_VISIBLE.0 | WS_BORDER.0 | WS_TABSTOP.0
        | 0x00200000 /* WS_VSCROLL */ | 0x0041 /* LBS_HASSTRINGS + LBS_NOTIFY */;
    unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("LISTBOX"),
            w!(""),
            windows::Win32::UI::WindowsAndMessaging::WINDOW_STYLE(style),
            x,
            y,
            w,
            h,
            Some(parent),
            Some(HMENU(id as isize as *mut c_void)),
            Some(GetModuleHandleW(None).unwrap().into()),
            None,
        )
        .unwrap()
    }
}

fn set_check(h: HWND, on: bool) {
    unsafe {
        SendMessageW(
            h,
            0x00F1,
            Some(WPARAM(if on { 1 } else { 0 })),
            Some(LPARAM(0)),
        );
    }
}
fn get_check(h: HWND) -> bool {
    unsafe { SendMessageW(h, 0x00F0, Some(WPARAM(0)), Some(LPARAM(0))).0 == 1 }
}
fn set_text(h: HWND, s: &str) {
    let w: Vec<u16> = s.encode_utf16().chain(once(0)).collect();
    unsafe {
        let _ = SetWindowTextW(h, PCWSTR(w.as_ptr()));
    }
}
fn get_text(h: HWND) -> String {
    unsafe {
        let len = GetWindowTextLengthW(h);
        if len <= 0 {
            return String::new();
        }
        let mut buf = vec![0u16; (len + 1) as usize];
        let got = GetWindowTextW(h, &mut buf);
        if got <= 0 {
            return String::new();
        }
        String::from_utf16_lossy(&buf[..got as usize])
    }
}
