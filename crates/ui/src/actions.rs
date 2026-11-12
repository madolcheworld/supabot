//! App-level actions and their default key bindings.
//!
//! GPUI dispatches input as *actions* bound to keystrokes, optionally scoped to
//! a `key_context`. Two contexts matter here:
//!
//! * `"Composer"` — `enter`→[`Send`], `shift-enter`→[`InsertNewline`]; scoped so
//!   Enter still inserts a newline in a card's multiline field.
//! * `"Overlay"` — arrow/enter navigation for the palette and the `@` picker.
//!
//! The text-editing actions (`Backspace`, `Left`, …) are bound with no context:
//! only the [`crate::widgets::text_input::TextInput`] listens for them, so a
//! global binding is harmless and lets every input share one keymap.

use gpui::{App, KeyBinding, actions};

actions!(
    swarmbot_ui,
    [
        /// Quit the application.
        Quit,
        /// Toggle the delegation-graph panel (`ctrl-g`).
        ToggleGraph,
        /// Toggle the command palette (`ctrl-k`).
        TogglePalette,
        /// Send the composer's text (Enter, in the Composer context).
        Send,
        /// Insert a newline instead of sending (Shift+Enter in the composer;
        /// Enter in a multiline card field).
        InsertNewline,
        /// Close whatever overlay is up (Escape): palette, then mention picker.
        CancelOverlay,
        /// Move the palette / picker selection down.
        SelectNext,
        /// Move the palette / picker selection up.
        SelectPrev,
        /// Activate the highlighted palette / picker entry.
        SelectConfirm,
        /// Focus the composer.
        FocusComposer,
        /// Interrupt the running turn of the open conversation.
        StopTurn,
    ]
);

actions!(
    text_input,
    [
        Backspace,
        Delete,
        Left,
        Right,
        Up,
        Down,
        SelectLeft,
        SelectRight,
        SelectAll,
        Home,
        End,
        Paste,
        Cut,
        Copy,
    ]
);

/// Register every default key binding. Called once at launch, before the first
/// window opens, so all windows inherit them.
pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        // App-level, no context.
        KeyBinding::new("ctrl-q", Quit, None),
        KeyBinding::new("ctrl-g", ToggleGraph, None),
        KeyBinding::new("ctrl-k", TogglePalette, None),
        KeyBinding::new("ctrl-.", StopTurn, None),
        // Escape is handled only at the root, which is the last element on the
        // focus path, so it always gets the event: it closes the palette first,
        // then the composer's mention picker.
        KeyBinding::new("escape", CancelOverlay, None),
        // Composer: Enter sends, Shift+Enter is a newline, and the arrows drive
        // the mention picker when it is open (the composer forwards them to the
        // caret when it is not). These are scoped to "Composer" so they beat the
        // global editing bindings only inside the composer.
        KeyBinding::new("enter", Send, Some("Composer")),
        KeyBinding::new("shift-enter", InsertNewline, Some("Composer")),
        KeyBinding::new("up", SelectPrev, Some("Composer")),
        KeyBinding::new("down", SelectNext, Some("Composer")),
        // Palette: its own focused overlay with arrow/enter navigation.
        KeyBinding::new("up", SelectPrev, Some("Palette")),
        KeyBinding::new("down", SelectNext, Some("Palette")),
        KeyBinding::new("enter", SelectConfirm, Some("Palette")),
        KeyBinding::new("ctrl-n", SelectNext, Some("Palette")),
        KeyBinding::new("ctrl-p", SelectPrev, Some("Palette")),
        // Text editing (only the TextInput listens). Bound for both the Linux
        // `ctrl-` and the macOS `cmd-` modifiers so the app behaves on either.
        // The bare arrows move the caret in a generic field; inside the
        // composer/palette the scoped bindings above take precedence.
        KeyBinding::new("backspace", Backspace, None),
        KeyBinding::new("delete", Delete, None),
        KeyBinding::new("left", Left, None),
        KeyBinding::new("right", Right, None),
        KeyBinding::new("up", Up, None),
        KeyBinding::new("down", Down, None),
        KeyBinding::new("shift-left", SelectLeft, None),
        KeyBinding::new("shift-right", SelectRight, None),
        KeyBinding::new("ctrl-a", SelectAll, None),
        KeyBinding::new("cmd-a", SelectAll, None),
        KeyBinding::new("ctrl-c", Copy, None),
        KeyBinding::new("cmd-c", Copy, None),
        KeyBinding::new("ctrl-x", Cut, None),
        KeyBinding::new("cmd-x", Cut, None),
        KeyBinding::new("ctrl-v", Paste, None),
        KeyBinding::new("cmd-v", Paste, None),
        KeyBinding::new("home", Home, None),
        KeyBinding::new("end", End, None),
    ]);
}
