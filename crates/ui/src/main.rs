//! `swarmbot-ui` — the GPUI desktop app for SwarmBot.
//!
//! This binary is a one-line wrapper: everything lives in the `swarmbot_ui`
//! library so the reducer, the widgets and the screens are testable headlessly
//! (`#[gpui::test]`) and `--selftest` can drive them without a second copy of the
//! boot sequence. See [`swarmbot_ui`] for the module map and
//! [`../../../docs/ui.md`] for the design.

fn main() {
    let code = swarmbot_ui::run();
    if code != 0 {
        std::process::exit(code);
    }
}
