//! A multi-line text input, hand-built because GPUI 0.2.2 ships the framework,
//! not Zed's component library ([`../../../docs/ui.md`] §4).
//!
//! It is gpui's `examples/input.rs` generalised from one shaped line to many:
//! the content is split on `'\n'`, each line is shaped separately
//! ([`gpui::TextSystem::shape_line`] asserts its text has no newline), and the
//! caret, selection and IME geometry are computed per line. Editing moves by
//! **`char`**, not by grapheme cluster — the crate cannot depend on
//! `unicode-segmentation` (it is not in `[workspace.dependencies]`), so a
//! combining mark could be split from its base by a caret move. That is a stated
//! limitation, not a silent one.
//!
//! Regular character input arrives through the platform IME path
//! ([`EntityInputHandler::replace_text_in_range`]), wired up in
//! [`TextElement::paint`] via `window.handle_input`; this widget only binds the
//! *editing* actions (backspace, arrows, copy/paste, newline).

use std::ops::Range;

use gpui::{
    App, Bounds, ClipboardItem, Context, CursorStyle, Element, ElementId, ElementInputHandler,
    Entity, EntityInputHandler, FocusHandle, Focusable, GlobalElementId, IntoElement, LayoutId,
    MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, PaintQuad, Pixels, Point, Rgba,
    ShapedLine, SharedString, Style, TextRun, UTF16Selection, Window, div, fill, point, prelude::*,
    px, relative, rgb, size,
};

use crate::actions::{
    Backspace, Copy, Cut, Delete, Down, End, Home, InsertNewline, Left, Paste, Right, SelectAll,
    SelectLeft, SelectRight, Up,
};
use crate::theme;

/// Caret colour, and the selection highlight base (drawn at reduced opacity).
const CARET: u32 = 0xf0f0f0;
const SELECTION_BG: u32 = 0x2f6fdb;
/// The bullet a `secure` field shows per character (U+2022, 3 bytes in UTF-8).
const MASK: char = '\u{2022}';
const MASK_LEN: usize = 3;

/// A text field. Create with [`TextInput::new`] inside `cx.new(..)`; chain
/// [`TextInput::multiline`] / [`TextInput::secure`] / [`TextInput::disabled`] to
/// change its behaviour.
pub struct TextInput {
    focus_handle: FocusHandle,
    content: SharedString,
    placeholder: SharedString,
    selected_range: Range<usize>,
    selection_reversed: bool,
    marked_range: Option<Range<usize>>,
    is_selecting: bool,
    multiline: bool,
    secure: bool,
    disabled: bool,
    font_size: Pixels,
    line_height: Pixels,
    // Geometry recorded by the last paint, used for mouse/IME hit-testing.
    last_lines: Vec<ShapedLine>,
    last_starts: Vec<usize>,
    last_ranges: Vec<Range<usize>>,
    last_bounds: Option<Bounds<Pixels>>,
}

impl TextInput {
    /// A single-line field with the given placeholder.
    pub fn new(placeholder: &str, cx: &mut Context<Self>) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
            content: SharedString::default(),
            placeholder: SharedString::from(placeholder.to_string()),
            selected_range: 0..0,
            selection_reversed: false,
            marked_range: None,
            is_selecting: false,
            multiline: false,
            secure: false,
            disabled: false,
            font_size: px(14.0),
            line_height: px(20.0),
            last_lines: Vec::new(),
            last_starts: Vec::new(),
            last_ranges: Vec::new(),
            last_bounds: None,
        }
    }

    /// Allow newlines (Shift+Enter in the composer, Enter in a card field).
    pub fn multiline(mut self) -> Self {
        self.multiline = true;
        self
    }

    /// Mask the content with a bullet per character; used for secret fields.
    pub fn secure(mut self) -> Self {
        self.secure = true;
        self
    }

    /// Dim the field and mark it non-interactive (the composer when offline).
    pub fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }

    /// Flip the disabled flag on a live field. The composer was built before
    /// the worker's socket existed, so it is born disabled and must re-enable
    /// itself when the brain comes online — the builder cannot express that.
    /// Notifies only on an actual change: callers sync on every state notify,
    /// and a redundant repaint of a focused field is pure flicker risk.
    pub fn set_disabled(&mut self, disabled: bool, cx: &mut Context<Self>) {
        if self.disabled == disabled {
            return;
        }
        self.disabled = disabled;
        cx.notify();
    }

    pub fn set_font_size(mut self, size: Pixels) -> Self {
        self.font_size = size;
        self.line_height = size * 1.45;
        self
    }

    /// Give this field the keyboard focus.
    pub fn focus(&self, window: &mut Window) {
        window.focus(&self.focus_handle);
    }

    // --- public accessors used by the composer / cards --------------------

    pub fn content(&self) -> &str {
        &self.content
    }

    pub fn is_empty(&self) -> bool {
        self.content.is_empty()
    }

    /// The caret offset (a `char` boundary) into the content.
    pub fn cursor(&self) -> usize {
        self.cursor_offset()
    }

    /// Replace the byte `range` with `text` and put the caret after it. The
    /// range must sit on `char` boundaries (the mention picker only ever passes
    /// ranges it computed from `char_indices`).
    pub fn splice(&mut self, range: Range<usize>, text: &str, cx: &mut Context<Self>) {
        let start = range.start.min(self.content.len());
        let end = range.end.clamp(start, self.content.len());
        let next = self.content[0..start].to_owned() + text + &self.content[end..];
        self.content = SharedString::from(next);
        let caret = start + text.len();
        self.selected_range = caret..caret;
        self.marked_range = None;
        cx.notify();
    }

    /// Replace the whole content, caret to the end.
    pub fn set_content(&mut self, text: &str, cx: &mut Context<Self>) {
        self.content = SharedString::from(text.to_string());
        let end = self.content.len();
        self.selected_range = end..end;
        self.marked_range = None;
        cx.notify();
    }

    pub fn clear(&mut self, cx: &mut Context<Self>) {
        self.set_content("", cx);
    }

    /// Insert `text` at the caret (used to splice in a chosen `@mention`).
    pub fn insert_at_cursor(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.replace_text_in_range(None, text, window, cx);
    }

    /// Move the caret down/up one line, keeping the column. The composer calls
    /// this when the mention picker is closed so the arrows still move the caret
    /// inside a multi-line draft.
    pub fn move_vertically(&mut self, delta: i32, cx: &mut Context<Self>) {
        if !self.multiline || self.last_ranges.is_empty() {
            return;
        }
        let cursor = self.cursor_offset();
        let (line, local) = self.offset_to_line_local(cursor);
        let last = self.last_ranges.len() - 1;
        let target = ((line as i32) + delta).clamp(0, last as i32) as usize;
        if target == line {
            return;
        }
        let target_len = self.last_ranges[target].len();
        let offset = self.last_starts[target] + local.min(target_len);
        self.move_to(offset, cx);
    }

    // --- char-boundary helpers (no unicode-segmentation) ------------------

    fn prev_char(&self, offset: usize) -> usize {
        self.content
            .char_indices()
            .rev()
            .find_map(|(idx, _)| (idx < offset).then_some(idx))
            .unwrap_or(0)
    }

    fn next_char(&self, offset: usize) -> usize {
        self.content
            .char_indices()
            .find_map(|(idx, _)| (idx > offset).then_some(idx))
            .unwrap_or(self.content.len())
    }

    fn cursor_offset(&self) -> usize {
        if self.selection_reversed {
            self.selected_range.start
        } else {
            self.selected_range.end
        }
    }

    fn move_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        self.selected_range = offset..offset;
        cx.notify();
    }

    fn select_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        if self.selection_reversed {
            self.selected_range.start = offset;
        } else {
            self.selected_range.end = offset;
        }
        if self.selected_range.end < self.selected_range.start {
            self.selection_reversed = !self.selection_reversed;
            self.selected_range = self.selected_range.end..self.selected_range.start;
        }
        cx.notify();
    }

    /// Byte ranges of each line of `content`. Empty content yields one empty
    /// line so there is always something to shape and hit-test.
    fn line_ranges(content: &str) -> Vec<Range<usize>> {
        let mut ranges = Vec::new();
        let mut start = 0;
        for (idx, ch) in content.char_indices() {
            if ch == '\n' {
                ranges.push(start..idx);
                start = idx + ch.len_utf8();
            }
        }
        ranges.push(start..content.len());
        ranges
    }

    /// Which line a byte offset falls on, and its offset within that line, from
    /// the last painted ranges. An offset at a line end belongs to that line.
    fn offset_to_line_local(&self, offset: usize) -> (usize, usize) {
        Self::line_local_at(offset, &self.last_ranges)
    }

    fn line_local_at(offset: usize, ranges: &[Range<usize>]) -> (usize, usize) {
        for (i, range) in ranges.iter().enumerate() {
            if offset <= range.end {
                return (i, offset.saturating_sub(range.start));
            }
        }
        let last = ranges.len().saturating_sub(1);
        (
            last,
            offset.saturating_sub(ranges.get(last).map(|r| r.start).unwrap_or(0)),
        )
    }

    /// The byte offset within a shaped line's *display* text for a real byte
    /// offset. A secure field's display text is bullets, so the mapping goes
    /// through the character count.
    fn display_offset(&self, line_start: usize, byte_offset: usize) -> usize {
        if self.secure {
            let from = line_start.min(self.content.len());
            let to = byte_offset.clamp(from, self.content.len());
            self.content[from..to].chars().count() * MASK_LEN
        } else {
            byte_offset.saturating_sub(line_start)
        }
    }

    /// The real byte offset for a display offset within a line (inverse of
    /// [`Self::display_offset`]).
    fn real_offset(&self, line_start: usize, line_end: usize, display_offset: usize) -> usize {
        let from = line_start.min(self.content.len());
        let to = line_end.clamp(from, self.content.len());
        if self.secure {
            let chars = display_offset / MASK_LEN;
            self.content[from..to]
                .char_indices()
                .nth(chars)
                .map(|(idx, _)| from + idx)
                .unwrap_or(to)
        } else {
            (from + display_offset).min(to)
        }
    }

    fn offset_from_utf16(&self, offset: usize) -> usize {
        let mut utf8 = 0;
        let mut utf16 = 0;
        for ch in self.content.chars() {
            if utf16 >= offset {
                break;
            }
            utf16 += ch.len_utf16();
            utf8 += ch.len_utf8();
        }
        utf8
    }

    fn offset_to_utf16(&self, offset: usize) -> usize {
        let mut utf8 = 0;
        let mut utf16 = 0;
        for ch in self.content.chars() {
            if utf8 >= offset {
                break;
            }
            utf8 += ch.len_utf8();
            utf16 += ch.len_utf16();
        }
        utf16
    }

    fn range_to_utf16(&self, range: &Range<usize>) -> Range<usize> {
        self.offset_to_utf16(range.start)..self.offset_to_utf16(range.end)
    }

    fn range_from_utf16(&self, range: &Range<usize>) -> Range<usize> {
        self.offset_from_utf16(range.start)..self.offset_from_utf16(range.end)
    }

    /// The screen point (caret edge) for a byte offset, from the last paint.
    fn point_for_offset(&self, offset: usize, bounds: Bounds<Pixels>) -> Point<Pixels> {
        if self.last_lines.is_empty() {
            return bounds.origin;
        }
        let last = self.last_lines.len() - 1;
        let (line, local) = self.offset_to_line_local(offset);
        let line = line.min(last);
        let line_start = self.last_ranges[line].start;
        let x = bounds.left()
            + self.last_lines[line]
                .x_for_index(self.display_offset(line_start, line_start + local));
        let y = bounds.top() + self.line_height * line;
        point(x, y)
    }

    // --- editing actions --------------------------------------------------

    fn backspace(&mut self, _: &Backspace, window: &mut Window, cx: &mut Context<Self>) {
        if self.disabled {
            return;
        }
        if self.selected_range.is_empty() {
            self.select_to(self.prev_char(self.cursor_offset()), cx);
        }
        self.replace_text_in_range(None, "", window, cx);
    }

    fn delete(&mut self, _: &Delete, window: &mut Window, cx: &mut Context<Self>) {
        if self.disabled {
            return;
        }
        if self.selected_range.is_empty() {
            self.select_to(self.next_char(self.cursor_offset()), cx);
        }
        self.replace_text_in_range(None, "", window, cx);
    }

    fn left(&mut self, _: &Left, _: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            self.move_to(self.prev_char(self.cursor_offset()), cx);
        } else {
            self.move_to(self.selected_range.start, cx);
        }
    }

    fn right(&mut self, _: &Right, _: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            self.move_to(self.next_char(self.selected_range.end), cx);
        } else {
            self.move_to(self.selected_range.end, cx);
        }
    }

    fn up(&mut self, _: &Up, _: &mut Window, cx: &mut Context<Self>) {
        self.move_vertically(-1, cx);
    }

    fn down(&mut self, _: &Down, _: &mut Window, cx: &mut Context<Self>) {
        self.move_vertically(1, cx);
    }

    fn select_left(&mut self, _: &SelectLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.prev_char(self.cursor_offset()), cx);
    }

    fn select_right(&mut self, _: &SelectRight, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.next_char(self.cursor_offset()), cx);
    }

    fn select_all(&mut self, _: &SelectAll, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(0, cx);
        self.select_to(self.content.len(), cx);
    }

    fn home(&mut self, _: &Home, _: &mut Window, cx: &mut Context<Self>) {
        if self.multiline && !self.last_ranges.is_empty() {
            let (line, _) = self.offset_to_line_local(self.cursor_offset());
            self.move_to(self.last_ranges[line].start, cx);
        } else {
            self.move_to(0, cx);
        }
    }

    fn end(&mut self, _: &End, _: &mut Window, cx: &mut Context<Self>) {
        if self.multiline && !self.last_ranges.is_empty() {
            let (line, _) = self.offset_to_line_local(self.cursor_offset());
            self.move_to(self.last_ranges[line].end, cx);
        } else {
            self.move_to(self.content.len(), cx);
        }
    }

    fn insert_newline(&mut self, _: &InsertNewline, window: &mut Window, cx: &mut Context<Self>) {
        if self.disabled || !self.multiline {
            return;
        }
        self.replace_text_in_range(None, "\n", window, cx);
    }

    fn paste(&mut self, _: &Paste, window: &mut Window, cx: &mut Context<Self>) {
        if self.disabled {
            return;
        }
        if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
            let text = if self.multiline {
                text
            } else {
                text.replace('\n', " ")
            };
            self.replace_text_in_range(None, &text, window, cx);
        }
    }

    fn copy(&mut self, _: &Copy, _: &mut Window, cx: &mut Context<Self>) {
        if !self.selected_range.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(
                self.content[self.selected_range.clone()].to_string(),
            ));
        }
    }

    fn cut(&mut self, _: &Cut, window: &mut Window, cx: &mut Context<Self>) {
        if self.disabled {
            return;
        }
        if !self.selected_range.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(
                self.content[self.selected_range.clone()].to_string(),
            ));
            self.replace_text_in_range(None, "", window, cx);
        }
    }

    fn on_mouse_down(&mut self, event: &MouseDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.disabled {
            return;
        }
        self.is_selecting = true;
        let index = self.index_for_position(event.position);
        if event.modifiers.shift {
            self.select_to(index, cx);
        } else {
            self.move_to(index, cx);
        }
    }

    fn on_mouse_up(&mut self, _: &MouseUpEvent, _: &mut Window, _: &mut Context<Self>) {
        self.is_selecting = false;
    }

    fn on_mouse_move(&mut self, event: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.is_selecting && !self.disabled {
            let index = self.index_for_position(event.position);
            self.select_to(index, cx);
        }
    }

    /// Map a window point to a byte offset in `content`, via the last painted
    /// geometry. Returns 0 before the first paint.
    fn index_for_position(&self, position: Point<Pixels>) -> usize {
        let Some(bounds) = self.last_bounds else {
            return 0;
        };
        if self.last_lines.is_empty() {
            return 0;
        }
        if position.y < bounds.top() {
            return 0;
        }
        if position.y > bounds.bottom() {
            return self.content.len();
        }
        let dy = f32::from(position.y - bounds.top());
        let lh = f32::from(self.line_height).max(1.0);
        let last = self.last_lines.len() as i32 - 1;
        let line = ((dy / lh).floor() as i32).clamp(0, last) as usize;
        let display = self.last_lines[line].closest_index_for_x(position.x - bounds.left());
        self.real_offset(
            self.last_ranges[line].start,
            self.last_ranges[line].end,
            display,
        )
    }
}

impl EntityInputHandler for TextInput {
    fn text_for_range(
        &mut self,
        range_utf16: Range<usize>,
        actual_range: &mut Option<Range<usize>>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<String> {
        let range = self.range_from_utf16(&range_utf16);
        actual_range.replace(self.range_to_utf16(&range));
        Some(self.content[range].to_string())
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        Some(UTF16Selection {
            range: self.range_to_utf16(&self.selected_range),
            reversed: self.selection_reversed,
        })
    }

    fn marked_text_range(
        &self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Range<usize>> {
        self.marked_range
            .as_ref()
            .map(|range| self.range_to_utf16(range))
    }

    fn unmark_text(&mut self, _window: &mut Window, _cx: &mut Context<Self>) {
        self.marked_range = None;
    }

    fn replace_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.disabled {
            return;
        }
        let range = range_utf16
            .as_ref()
            .map(|r| self.range_from_utf16(r))
            .or(self.marked_range.clone())
            .unwrap_or(self.selected_range.clone());
        self.content = SharedString::from(
            self.content[0..range.start].to_owned() + new_text + &self.content[range.end..],
        );
        let caret = range.start + new_text.len();
        self.selected_range = caret..caret;
        self.marked_range.take();
        cx.notify();
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        new_selected_range_utf16: Option<Range<usize>>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.disabled {
            return;
        }
        let range = range_utf16
            .as_ref()
            .map(|r| self.range_from_utf16(r))
            .or(self.marked_range.clone())
            .unwrap_or(self.selected_range.clone());
        self.content = SharedString::from(
            self.content[0..range.start].to_owned() + new_text + &self.content[range.end..],
        );
        self.marked_range = if new_text.is_empty() {
            None
        } else {
            Some(range.start..range.start + new_text.len())
        };
        self.selected_range = new_selected_range_utf16
            .as_ref()
            .map(|r| self.range_from_utf16(r))
            .map(|new_range| new_range.start + range.start..new_range.end + range.end)
            .unwrap_or_else(|| range.start + new_text.len()..range.start + new_text.len());
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        bounds: Bounds<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let range = self.range_from_utf16(&range_utf16);
        let start = self.point_for_offset(range.start, bounds);
        let end = self.point_for_offset(range.end, bounds);
        Some(Bounds::from_corners(start, end))
    }

    fn character_index_for_point(
        &mut self,
        point: Point<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        Some(self.offset_to_utf16(self.index_for_position(point)))
    }
}

/// The painted element: shapes the lines, draws the caret and selection, and
/// registers the IME input handler.
struct TextElement {
    input: Entity<TextInput>,
}

struct PrepaintState {
    lines: Vec<ShapedLine>,
    starts: Vec<usize>,
    ranges: Vec<Range<usize>>,
    line_height: Pixels,
    cursor: Option<PaintQuad>,
    selection: Vec<PaintQuad>,
}

impl IntoElement for TextElement {
    type Element = Self;
    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for TextElement {
    type RequestLayoutState = ();
    type PrepaintState = PrepaintState;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let input = self.input.read(cx);
        let line_count = TextInput::line_ranges(&input.content).len().max(1);
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        style.size.height = (input.line_height * line_count).into();
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let input = self.input.read(cx);
        let line_height = input.line_height;
        let font_size = input.font_size;
        let base_color: Rgba = if input.disabled {
            rgb(theme::TEXT_DIM)
        } else {
            rgb(theme::TEXT)
        };
        let text_style = window.text_style();
        let font = text_style.font();

        let ranges = TextInput::line_ranges(&input.content);
        let empty = input.content.is_empty();
        let mut lines = Vec::with_capacity(ranges.len());
        let mut starts = Vec::with_capacity(ranges.len());
        for range in &ranges {
            let (display_text, color): (SharedString, Rgba) = if empty {
                (input.placeholder.clone(), rgb(theme::TEXT_DIM))
            } else if input.secure {
                let chars = input.content[range.clone()].chars().count();
                (MASK.to_string().repeat(chars).into(), base_color)
            } else {
                (input.content[range.clone()].to_string().into(), base_color)
            };
            let run = TextRun {
                len: display_text.len(),
                font: font.clone(),
                color: color.into(),
                background_color: None,
                underline: None,
                strikethrough: None,
            };
            let shaped = window
                .text_system()
                .shape_line(display_text, font_size, &[run], None);
            lines.push(shaped);
            starts.push(range.start);
        }

        // Selection highlight, one quad per overlapped line.
        let selection_color = Rgba {
            a: 0.45,
            ..rgb(SELECTION_BG)
        };
        let mut selection = Vec::new();
        if !input.selected_range.is_empty() {
            for (line, range) in ranges.iter().enumerate() {
                let from = input.selected_range.start.max(range.start);
                let to = input.selected_range.end.min(range.end);
                if from >= to {
                    continue;
                }
                let x0 = lines[line].x_for_index(input.display_offset(range.start, from));
                let x1 = lines[line].x_for_index(input.display_offset(range.start, to));
                let y = bounds.top() + line_height * line;
                selection.push(fill(
                    Bounds::from_corners(
                        point(bounds.left() + x0, y),
                        point(bounds.left() + x1, y + line_height),
                    ),
                    selection_color,
                ));
            }
        }

        // Caret, only when there is no selection.
        let cursor = if empty || !input.selected_range.is_empty() || lines.is_empty() {
            None
        } else {
            let offset = input.cursor_offset();
            let (line, local) = TextInput::line_local_at(offset, &ranges);
            let line = line.min(lines.len() - 1);
            let line_start = ranges[line].start;
            let x = lines[line].x_for_index(input.display_offset(line_start, line_start + local));
            let y = bounds.top() + line_height * line;
            Some(fill(
                Bounds::new(point(bounds.left() + x, y), size(px(2.0), line_height)),
                rgb(CARET),
            ))
        };

        PrepaintState {
            lines,
            starts,
            ranges,
            line_height,
            cursor,
            selection,
        }
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let focus_handle = self.input.read(cx).focus_handle.clone();
        let disabled = self.input.read(cx).disabled;
        if !disabled {
            window.handle_input(
                &focus_handle,
                ElementInputHandler::new(bounds, self.input.clone()),
                cx,
            );
        }

        for quad in prepaint.selection.drain(..) {
            window.paint_quad(quad);
        }

        let line_height = prepaint.line_height;
        for (line, shaped) in prepaint.lines.iter().enumerate() {
            let origin = point(bounds.left(), bounds.top() + line_height * line);
            let _ = shaped.paint(origin, line_height, window, cx);
        }

        if !disabled
            && focus_handle.is_focused(window)
            && let Some(cursor) = prepaint.cursor.take()
        {
            window.paint_quad(cursor);
        }

        let lines = std::mem::take(&mut prepaint.lines);
        let starts = std::mem::take(&mut prepaint.starts);
        let ranges = std::mem::take(&mut prepaint.ranges);
        self.input.update(cx, |input, _| {
            input.last_lines = lines;
            input.last_starts = starts;
            input.last_ranges = ranges;
            input.last_bounds = Some(bounds);
        });
    }
}

impl Render for TextInput {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let focused = self.focus_handle.is_focused(window);
        div()
            .flex()
            .key_context("TextInput")
            .track_focus(&self.focus_handle(cx))
            .cursor(CursorStyle::IBeam)
            .on_action(cx.listener(Self::backspace))
            .on_action(cx.listener(Self::delete))
            .on_action(cx.listener(Self::left))
            .on_action(cx.listener(Self::right))
            .on_action(cx.listener(Self::up))
            .on_action(cx.listener(Self::down))
            .on_action(cx.listener(Self::select_left))
            .on_action(cx.listener(Self::select_right))
            .on_action(cx.listener(Self::select_all))
            .on_action(cx.listener(Self::home))
            .on_action(cx.listener(Self::end))
            .on_action(cx.listener(Self::insert_newline))
            .on_action(cx.listener(Self::paste))
            .on_action(cx.listener(Self::cut))
            .on_action(cx.listener(Self::copy))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .w_full()
            .rounded_md()
            .border_1()
            .border_color(rgb(if focused {
                theme::ACCENT
            } else {
                theme::BORDER
            }))
            .bg(rgb(theme::CODE_BG))
            .px_2()
            .py_1()
            .text_size(self.font_size)
            .line_height(self.line_height)
            .when(self.disabled, |d| d.opacity(0.55))
            .child(TextElement { input: cx.entity() })
    }
}

impl Focusable for TextInput {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}
