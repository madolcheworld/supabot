//! A small markdown renderer: `pulldown-cmark` → a testable IR ([`MdBlock`]) →
//! GPUI elements ([`render`]).
//!
//! A bot's reply is markdown, and rendering it as raw text is the difference
//! between a transcript and a wall ([`../../../docs/ui.md`] §4). The parse step
//! is split from the render step on purpose: [`parse`] is a pure function a
//! `#[test]` asserts on without a window, and [`render`] only turns that
//! IR into styled `div`s.
//!
//! Scope, stated plainly: this handles headings, paragraphs, **bold**, *italic*,
//! `inline code`, fenced code blocks, bullet/numbered lists (nesting shown by
//! indentation), block quotes, rules and links. Tables, images, footnotes, HTML
//! and math are **not** rendered — an unhandled event is dropped, so such input
//! degrades to its plain text rather than failing. Links open with `cx.open_url`.

use gpui::{
    Div, FontStyle, FontWeight, HighlightStyle, InteractiveText, SharedString, StyledText,
    UnderlineStyle, div, prelude::*, px, rgb,
};
use pulldown_cmark::{CodeBlockKind, Event, Parser, Tag, TagEnd};

use crate::theme;

/// The style of one inline run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MdStyle {
    Normal,
    Bold,
    Italic,
    Code,
    Link(String),
}

/// One inline run of text with a style.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Span {
    pub text: String,
    pub style: MdStyle,
}

impl Span {
    pub fn new(text: impl Into<String>, style: MdStyle) -> Self {
        Self {
            text: text.into(),
            style,
        }
    }
}

/// One block of the rendered document.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MdBlock {
    Heading {
        level: u8,
        spans: Vec<Span>,
    },
    Paragraph {
        spans: Vec<Span>,
    },
    Code {
        lang: Option<String>,
        text: String,
    },
    Bullet {
        depth: usize,
        spans: Vec<Span>,
    },
    Numbered {
        depth: usize,
        n: usize,
        spans: Vec<Span>,
    },
    Quote {
        spans: Vec<Span>,
    },
    Rule,
}

/// The plain-text content of a block, for tests and search previews.
pub fn block_text(block: &MdBlock) -> String {
    match block {
        MdBlock::Heading { spans, .. }
        | MdBlock::Paragraph { spans }
        | MdBlock::Bullet { spans, .. }
        | MdBlock::Numbered { spans, .. }
        | MdBlock::Quote { spans } => spans.iter().map(|s| s.text.as_str()).collect(),
        MdBlock::Code { text, .. } => text.clone(),
        MdBlock::Rule => String::new(),
    }
}

/// Parse markdown into blocks. Pure; never panics on unusual input (unknown
/// events are ignored, so their text still surfaces via [`Event::Text`]).
pub fn parse(markdown: &str) -> Vec<MdBlock> {
    #[derive(Clone, Copy, PartialEq)]
    enum Kind {
        Para,
        Heading(u8),
        Code,
        Quote,
        Item {
            ordered: bool,
            index: usize,
            depth: usize,
        },
    }
    struct Ctx {
        kind: Kind,
        inline: Vec<Span>,
        code: String,
        lang: Option<String>,
    }
    impl Ctx {
        fn new(kind: Kind) -> Self {
            Self {
                kind,
                inline: Vec::new(),
                code: String::new(),
                lang: None,
            }
        }
    }

    let mut blocks: Vec<MdBlock> = Vec::new();
    let mut stack: Vec<Ctx> = Vec::new();
    let mut styles: Vec<MdStyle> = vec![MdStyle::Normal];
    // (ordered, next_index) per open list; the depth is the stack length - 1.
    let mut lists: Vec<(bool, usize)> = Vec::new();

    // A paragraph/heading/quote closes into its parent when the parent is a
    // quote or a list item (markdown wraps those in a paragraph we do not want
    // emitted as a separate block).
    let merges_into_parent = |parent: &Ctx| matches!(parent.kind, Kind::Quote | Kind::Item { .. });

    for event in Parser::new(markdown) {
        match event {
            Event::Start(tag) => match tag {
                Tag::Paragraph => stack.push(Ctx::new(Kind::Para)),
                Tag::Heading { level, .. } => {
                    stack.push(Ctx::new(Kind::Heading(level as u8)));
                }
                Tag::CodeBlock(kind) => {
                    let mut ctx = Ctx::new(Kind::Code);
                    ctx.lang = match kind {
                        CodeBlockKind::Fenced(info) => {
                            let first = info
                                .to_string()
                                .split_whitespace()
                                .next()
                                .unwrap_or("")
                                .to_string();
                            (!first.is_empty()).then_some(first)
                        }
                        CodeBlockKind::Indented => None,
                    };
                    stack.push(ctx);
                }
                Tag::BlockQuote(_) => stack.push(Ctx::new(Kind::Quote)),
                Tag::List(start) => lists.push((start.is_some(), start.unwrap_or(1) as usize)),
                Tag::Item => {
                    let (ordered, index) = lists.last().copied().unwrap_or((false, 1));
                    let depth = lists.len().saturating_sub(1);
                    stack.push(Ctx::new(Kind::Item {
                        ordered,
                        index,
                        depth,
                    }));
                }
                Tag::Strong => styles.push(MdStyle::Bold),
                Tag::Emphasis => styles.push(MdStyle::Italic),
                Tag::Link { dest_url, .. } => styles.push(MdStyle::Link(dest_url.to_string())),
                // Tables, images, HTML, math, footnotes: not rendered; their
                // text still arrives as Event::Text.
                _ => {}
            },
            Event::End(end) => match end {
                TagEnd::Paragraph => {
                    if let Some(ctx) = stack.pop() {
                        let spans = ctx.inline;
                        match stack.last_mut() {
                            Some(parent) if merges_into_parent(parent) => {
                                parent.inline.extend(spans)
                            }
                            _ => blocks.push(MdBlock::Paragraph { spans }),
                        }
                    }
                }
                TagEnd::Heading(_) => {
                    if let Some(ctx) = stack.pop() {
                        let level = match ctx.kind {
                            Kind::Heading(level) => level,
                            _ => 1,
                        };
                        let spans = ctx.inline;
                        match stack.last_mut() {
                            Some(parent) if merges_into_parent(parent) => {
                                parent.inline.extend(spans)
                            }
                            _ => blocks.push(MdBlock::Heading { level, spans }),
                        }
                    }
                }
                TagEnd::CodeBlock => {
                    if let Some(ctx) = stack.pop() {
                        blocks.push(MdBlock::Code {
                            lang: ctx.lang,
                            text: ctx.code.trim_end_matches('\n').to_string(),
                        });
                    }
                }
                TagEnd::BlockQuote(_) => {
                    if let Some(ctx) = stack.pop() {
                        let spans = ctx.inline;
                        match stack.last_mut() {
                            Some(parent) if merges_into_parent(parent) => {
                                parent.inline.extend(spans)
                            }
                            _ => blocks.push(MdBlock::Quote { spans }),
                        }
                    }
                }
                TagEnd::Item => {
                    if let Some(ctx) = stack.pop() {
                        let (ordered, index, depth) = match ctx.kind {
                            Kind::Item {
                                ordered,
                                index,
                                depth,
                            } => (ordered, index, depth),
                            _ => (false, 1, 0),
                        };
                        let spans = ctx.inline;
                        blocks.push(if ordered {
                            MdBlock::Numbered {
                                depth,
                                n: index,
                                spans,
                            }
                        } else {
                            MdBlock::Bullet { depth, spans }
                        });
                        if ordered && let Some(top) = lists.last_mut() {
                            top.1 = index + 1;
                        }
                    }
                }
                TagEnd::List(_) => {
                    lists.pop();
                }
                TagEnd::Strong | TagEnd::Emphasis | TagEnd::Link if styles.len() > 1 => {
                    styles.pop();
                }
                _ => {}
            },
            Event::Text(text) => {
                let text = text.to_string();
                if let Some(ctx) = stack.last_mut() {
                    if ctx.kind == Kind::Code {
                        ctx.code.push_str(&text);
                    } else {
                        ctx.inline.push(Span::new(
                            text,
                            styles.last().cloned().unwrap_or(MdStyle::Normal),
                        ));
                    }
                } else {
                    blocks.push(MdBlock::Paragraph {
                        spans: vec![Span::new(
                            text,
                            styles.last().cloned().unwrap_or(MdStyle::Normal),
                        )],
                    });
                }
            }
            Event::Code(code) => {
                if let Some(ctx) = stack.last_mut() {
                    ctx.inline.push(Span::new(code.to_string(), MdStyle::Code));
                }
            }
            Event::SoftBreak => {
                if let Some(ctx) = stack.last_mut()
                    && ctx.kind != Kind::Code
                {
                    ctx.inline.push(Span::new(
                        " ",
                        styles.last().cloned().unwrap_or(MdStyle::Normal),
                    ));
                }
            }
            Event::HardBreak => {
                if let Some(ctx) = stack.last_mut() {
                    ctx.inline.push(Span::new(
                        "\n",
                        styles.last().cloned().unwrap_or(MdStyle::Normal),
                    ));
                }
            }
            Event::Rule => blocks.push(MdBlock::Rule),
            _ => {}
        }
    }
    blocks
}

/// Parse and render in one call.
pub fn render_markdown(markdown: &str) -> Div {
    render(&parse(markdown))
}

/// Turn parsed blocks into a vertical stack of GPUI elements. The container
/// sets the reading size for the transcript (headings and code override it
/// with their own), so bubbles match the text scale of the rest of the app
/// instead of GPUI's 16px default.
pub fn render(blocks: &[MdBlock]) -> Div {
    let mut ids = 0usize;
    let mut container = div().flex().flex_col().gap_2().w_full().text_size(px(13.5));
    for block in blocks {
        container = container.child(render_block(block, &mut ids));
    }
    container
}

fn render_block(block: &MdBlock, ids: &mut usize) -> Div {
    match block {
        MdBlock::Heading { level, spans } => {
            let size = match level {
                1 => px(22.0),
                2 => px(19.0),
                3 => px(17.0),
                _ => px(15.0),
            };
            inline_row(spans, ids)
                .text_size(size)
                .font_weight(FontWeight::BOLD)
                .text_color(rgb(theme::TEXT))
        }
        MdBlock::Paragraph { spans } => inline_row(spans, ids).text_color(rgb(theme::TEXT)),
        MdBlock::Code { text, .. } => div().w_full().child(
            div()
                .id(next_id(ids, "md-code-block"))
                .w_full()
                .rounded_md()
                .bg(rgb(theme::CODE_BG))
                .border_1()
                .border_color(rgb(theme::BORDER))
                .px_2()
                .py_1()
                .font_family(MONO)
                .text_size(px(12.5))
                .text_color(rgb(theme::TEXT))
                .overflow_x_scroll()
                .child(text.clone()),
        ),
        MdBlock::Bullet { depth, spans } => list_row("•", *depth, spans, ids),
        MdBlock::Numbered { depth, n, spans } => list_row(&format!("{n}."), *depth, spans, ids),
        MdBlock::Quote { spans } => inline_row(spans, ids)
            .border_l_2()
            .border_color(rgb(theme::BORDER))
            .pl_2()
            .text_color(rgb(theme::TEXT_MUTED)),
        MdBlock::Rule => div().w_full().h(px(1.0)).bg(rgb(theme::BORDER)),
    }
}

fn list_row(marker: &str, depth: usize, spans: &[Span], ids: &mut usize) -> Div {
    div()
        .w_full()
        .flex()
        .items_start()
        .gap_2()
        .pl(px(depth as f32 * 14.0))
        .child(
            div()
                .flex_none()
                .text_color(rgb(theme::TEXT_MUTED))
                .child(marker.to_string()),
        )
        .child(inline_row(spans, ids).flex_1().min_w_0())
}

/// A paragraph rendered as wrapped text with inline styles and interactive links.
fn inline_row(spans: &[Span], ids: &mut usize) -> Div {
    if spans.is_empty() {
        return div().w_full();
    }

    let mut full_text = String::new();
    let mut highlights = Vec::new();
    let mut links = Vec::new();

    for span in spans {
        if span.text.is_empty() {
            continue;
        }
        let start = full_text.len();
        full_text.push_str(&span.text);
        let end = full_text.len();
        let range = start..end;

        match &span.style {
            MdStyle::Normal => {}
            MdStyle::Bold => {
                highlights.push((
                    range,
                    HighlightStyle {
                        font_weight: Some(FontWeight::BOLD),
                        ..Default::default()
                    },
                ));
            }
            MdStyle::Italic => {
                highlights.push((
                    range,
                    HighlightStyle {
                        font_style: Some(FontStyle::Italic),
                        ..Default::default()
                    },
                ));
            }
            MdStyle::Code => {
                highlights.push((
                    range,
                    HighlightStyle {
                        background_color: Some(rgb(theme::CODE_BG).into()),
                        ..Default::default()
                    },
                ));
            }
            MdStyle::Link(url) => {
                highlights.push((
                    range.clone(),
                    HighlightStyle {
                        color: Some(rgb(theme::ACCENT_HOVER).into()),
                        underline: Some(UnderlineStyle {
                            thickness: px(1.0),
                            color: Some(rgb(theme::ACCENT_HOVER).into()),
                            wavy: false,
                        }),
                        ..Default::default()
                    },
                ));
                links.push((range, url.clone()));
            }
        }
    }

    if full_text.is_empty() {
        return div().w_full();
    }

    let styled = StyledText::new(full_text).with_highlights(highlights);

    if links.is_empty() {
        div().w_full().child(styled)
    } else {
        let urls: Vec<String> = links.iter().map(|(_, u)| u.clone()).collect();
        let ranges: Vec<std::ops::Range<usize>> = links.into_iter().map(|(r, _)| r).collect();
        let link_id = next_id(ids, "md-interactive");
        let interactive =
            InteractiveText::new(link_id, styled).on_click(ranges, move |ix, _, cx| {
                if let Some(url) = urls.get(ix) {
                    cx.open_url(url);
                }
            });
        div().w_full().child(interactive)
    }
}

fn next_id(ids: &mut usize, prefix: &str) -> SharedString {
    *ids += 1;
    SharedString::from(format!("{prefix}-{}", *ids))
}

/// The monospace family used for code. GPUI resolves it through `font-kit`; if a
/// machine lacks it, GPUI falls back to the default face rather than failing, so
/// this is a preference, not a hard requirement.
const MONO: &str = "DejaVu Sans Mono";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_headings_and_inline_styles() {
        let blocks = parse("# Title\n\nSome **bold** and *italic* and `code`.");
        assert!(matches!(&blocks[0], MdBlock::Heading { level: 1, .. }));
        assert_eq!(block_text(&blocks[0]), "Title");
        let spans = match &blocks[1] {
            MdBlock::Paragraph { spans } => spans,
            other => panic!("expected a paragraph, got {other:?}"),
        };
        assert!(
            spans
                .iter()
                .any(|s| s.style == MdStyle::Bold && s.text == "bold")
        );
        assert!(
            spans
                .iter()
                .any(|s| s.style == MdStyle::Italic && s.text == "italic")
        );
        assert!(
            spans
                .iter()
                .any(|s| s.style == MdStyle::Code && s.text == "code")
        );
    }

    #[test]
    fn parses_fenced_code_with_language() {
        let blocks = parse("```rust\nfn main() {}\n```");
        match &blocks[0] {
            MdBlock::Code { lang, text } => {
                assert_eq!(lang.as_deref(), Some("rust"));
                assert_eq!(text, "fn main() {}");
            }
            other => panic!("expected code, got {other:?}"),
        }
    }

    #[test]
    fn parses_bullet_and_numbered_lists() {
        let blocks = parse("- one\n- two\n\n1. first\n2. second");
        let bullets = blocks
            .iter()
            .filter(|b| matches!(b, MdBlock::Bullet { .. }))
            .count();
        let numbered: Vec<_> = blocks
            .iter()
            .filter_map(|b| match b {
                MdBlock::Numbered { n, .. } => Some(*n),
                _ => None,
            })
            .collect();
        assert_eq!(bullets, 2);
        assert_eq!(numbered, vec![1, 2], "the running number advances");
    }

    #[test]
    fn parses_links_and_keeps_the_url() {
        let blocks = parse("see [the docs](https://example.invalid/x)");
        let spans = match &blocks[0] {
            MdBlock::Paragraph { spans } => spans,
            other => panic!("expected a paragraph, got {other:?}"),
        };
        assert!(spans.iter().any(
            |s| matches!(&s.style, MdStyle::Link(u) if u == "https://example.invalid/x")
                && s.text == "the docs"
        ));
    }

    #[test]
    fn unrendered_input_degrades_to_text_without_panicking() {
        // Raw HTML and tables are out of scope; they must not panic and their
        // text should still surface.
        let blocks = parse("<b>hi</b> plain\n\n| a | b |\n| - | - |\n| 1 | 2 |");
        let all: String = blocks.iter().map(block_text).collect::<Vec<_>>().join(" ");
        assert!(all.contains("plain"), "text survives: {all:?}");
        // render() must also not panic on whatever parse produced.
        let _ = render(&blocks);
    }

    #[test]
    fn renders_rich_blocks_with_styled_text_and_links() {
        let text = "Here is **bold**, *italic*, `inline code`, and a [link](https://example.com).\n\n```rust\nlet x = 42;\n```";
        let blocks = parse(text);
        assert_eq!(blocks.len(), 2);
        let _ = render(&blocks);
    }
}
