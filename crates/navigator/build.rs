//! Embed a Windows manifest that declares a dependency on Common Controls
//! v6. Without it, `TaskDialogIndirect` fails at runtime and our modern
//! widgets fall back to the classic look. DPI awareness is set to
//! per-monitor-v2 so the listview doesn't blur on high-DPI displays.

use embed_manifest::manifest::{ActiveCodePage, DpiAwareness, SupportedOS};
use embed_manifest::{embed_manifest, new_manifest};

fn main() {
    if std::env::var_os("CARGO_CFG_WINDOWS").is_none() {
        return;
    }
    let manifest = new_manifest("Anthropic.Navigator")
        .active_code_page(ActiveCodePage::Utf8)
        .dpi_awareness(DpiAwareness::PerMonitorV2)
        .supported_os(SupportedOS::Windows10..=SupportedOS::Windows10);
    embed_manifest(manifest).expect("embed manifest");
    println!("cargo:rerun-if-changed=build.rs");
}
