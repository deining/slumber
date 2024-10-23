use crate::{
    context::TuiContext,
    view::{
        common::scrollbar::Scrollbar,
        context::UpdateContext,
        draw::{Draw, DrawMetadata},
        event::{Event, EventHandler, Update},
    },
};
use ratatui::{
    buffer::Buffer,
    layout::{Layout, Rect},
    prelude::{Alignment, Constraint},
    style::Style,
    text::{Line, StyledGrapheme, Text},
    widgets::{Paragraph, ScrollbarOrientation},
    Frame,
};
use slumber_config::Action;
use std::{cell::Cell, cmp};
use unicode_width::UnicodeWidthStr;

/// A scrollable (but not editable) block of text. Internal state will be
/// updated on each render, to adjust to the text's width/height. Generally the
/// parent should be storing an instance of [Text] and passing the same value to
/// this on each render. Generating the `Text` could potentially be expensive
/// (especially if it includes syntax highlighting).
#[derive(derive_more::Debug, Default)]
pub struct TextWindow {
    /// Horizontal scroll
    offset_x: Cell<usize>,
    /// Vertical scroll
    offset_y: Cell<usize>,
    /// How wide is the full text content?
    text_width: Cell<usize>,
    /// How tall is the full text content?
    text_height: Cell<usize>,
    /// How wide is the visible text area, excluding gutter/scrollbars?
    window_width: Cell<usize>,
    /// How tall is the visible text area, exluding gutter/scrollbars?
    window_height: Cell<usize>,
}

#[derive(Clone)]
pub struct TextWindowProps<'a> {
    /// Text to render. We take a reference because this component tends to
    /// contain a lot of text, and we don't want to force a clone on render
    pub text: &'a Text<'a>,
    /// Extra text to render below the text window
    pub footer: Option<Text<'a>>,
    pub margins: ScrollbarMargins,
}

/// How far outside the text window should scrollbars be placed? Margin of
/// 0 uses the outermost row/column of the text area. Positive values
/// pushes the scrollbar outside the rendered outside, negative moves
/// it inside.
#[derive(Clone)]
pub struct ScrollbarMargins {
    pub right: i32,
    pub bottom: i32,
}

impl Default for ScrollbarMargins {
    fn default() -> Self {
        Self {
            right: 1,
            bottom: 1,
        }
    }
}

impl TextWindow {
    /// Get the final line that we can't scroll past. This will be the first
    /// line of the last page of text
    fn max_scroll_line(&self) -> usize {
        self.text_height
            .get()
            .saturating_sub(self.window_height.get())
    }

    /// Get the final column that we can't scroll (horizontally) past. This will
    /// be the left edge of the rightmost "page" of text
    fn max_scroll_column(&self) -> usize {
        self.text_width
            .get()
            .saturating_sub(self.window_width.get())
    }

    fn scroll_up(&mut self, lines: usize) {
        *self.offset_y.get_mut() = self.offset_y.get().saturating_sub(lines);
    }

    fn scroll_down(&mut self, lines: usize) {
        *self.offset_y.get_mut() =
            cmp::min(self.offset_y.get() + lines, self.max_scroll_line());
    }

    /// Scroll to a specific line number. The target line will end up as close
    /// to the top of the page as possible
    fn scroll_to(&mut self, line: usize) {
        *self.offset_y.get_mut() = cmp::min(line, self.max_scroll_line());
    }

    fn scroll_left(&mut self, columns: usize) {
        *self.offset_x.get_mut() = self.offset_x.get().saturating_sub(columns);
    }

    fn scroll_right(&mut self, columns: usize) {
        *self.offset_x.get_mut() =
            cmp::min(self.offset_x.get() + columns, self.max_scroll_column());
    }

    /// Ensure the scroll state is valid. Called on every render, in case the
    /// text size or draw area changed
    fn clamp_scroll(&self) {
        self.offset_x
            .set(cmp::min(self.offset_x.get(), self.max_scroll_column()));
        self.offset_y
            .set(cmp::min(self.offset_y.get(), self.max_scroll_line()));
    }

    /// Render the visible text into the window. The Paragraph widget provides
    /// all this functionality out of the box, but it needs an owned Text and
    /// we only have a reference. A clone could potentially be very expensive
    /// for a large body, so we use our own logic.
    fn render_chars<'a>(
        &self,
        text: &'a Text<'a>,
        buf: &mut Buffer,
        area: Rect,
    ) {
        let lines = text
            .lines
            .iter()
            .skip(self.offset_y.get())
            .take(self.window_height.get())
            .enumerate();
        for (y, line) in lines {
            let graphemes = line
                .styled_graphemes(Style::default())
                .skip(self.offset_x.get())
                .take(self.window_width.get());
            let mut x = 0;
            for StyledGrapheme { symbol, style } in graphemes {
                if x >= area.width {
                    break;
                }
                buf[(area.left() + x, area.top() + y as u16)]
                    .set_symbol(symbol)
                    .set_style(style);
                x += symbol.width() as u16;
            }
        }
    }
}

impl EventHandler for TextWindow {
    fn update(&mut self, _: &mut UpdateContext, event: Event) -> Update {
        let Some(action) = event.action() else {
            return Update::Propagate(event);
        };
        match action {
            Action::Up | Action::ScrollUp => self.scroll_up(1),
            Action::Down | Action::ScrollDown => self.scroll_down(1),
            Action::ScrollLeft => self.scroll_left(1),
            Action::ScrollRight => self.scroll_right(1),
            Action::PageUp => self.scroll_up(self.window_height.get()),
            Action::PageDown => self.scroll_down(self.window_height.get()),
            Action::Home => self.scroll_to(0),
            Action::End => self.scroll_to(usize::MAX),
            _ => return Update::Propagate(event),
        }
        Update::Consumed
    }
}

/// `T` has to be convertible to text to be drawn
impl<'a> Draw<TextWindowProps<'a>> for TextWindow {
    fn draw(
        &self,
        frame: &mut Frame,
        props: TextWindowProps<'a>,
        metadata: DrawMetadata,
    ) {
        let styles = &TuiContext::get().styles;

        // Assume no line wrapping when calculating line count
        // Note: Paragraph has methods for this, but that requires an owned copy
        // of Text, which involves a lot of cloning
        let text_height = props.text.lines.len();
        let text_width = props
            .text
            .lines
            .iter()
            .map(Line::width)
            .max()
            .unwrap_or_default();

        let [gutter_area, _, text_area] = Layout::horizontal([
            // Size gutter based on width of max line number
            Constraint::Length((text_height as f32).log10().floor() as u16 + 1),
            Constraint::Length(1), // Spacer
            Constraint::Min(0),
        ])
        .areas(metadata.area());
        let has_vertical_scroll = text_height > text_area.height as usize;
        let has_horizontal_scroll = text_width > text_area.width as usize;

        // Store text and window sizes for calculations in the update code
        self.text_width.set(text_width);
        self.text_height.set(text_height);
        self.window_width.set(text_area.width as usize);
        self.window_height.set(text_area.height as usize);

        // Scroll state could become invalid if window size or text changes
        self.clamp_scroll();

        // Draw line numbers in the gutter
        let first_line = self.offset_y.get() + 1;
        let last_line =
            cmp::min(first_line + self.window_height.get() - 1, text_height);
        frame.render_widget(
            Paragraph::new(
                (first_line..=last_line)
                    .map(|n| n.to_string().into())
                    .collect::<Vec<Line>>(),
            )
            .alignment(Alignment::Right)
            .style(styles.text_window.gutter),
            gutter_area,
        );

        // Draw the text content
        self.render_chars(props.text, frame.buffer_mut(), text_area);

        // Render the footer just below the text. If the text has maxed out the
        // possible area, this will render beyond that. A bit hacky but in
        // practice it works
        if let Some(footer) = props.footer {
            frame.render_widget(
                footer,
                Rect {
                    x: text_area.x,
                    y: text_area.y
                        + (cmp::min(
                            self.text_height.get(),
                            self.window_height.get(),
                        )) as u16,
                    width: text_area.width,
                    height: 1,
                },
            );
        }

        // Scrollbars
        if has_vertical_scroll {
            frame.render_widget(
                Scrollbar {
                    content_length: self.text_height.get(),
                    offset: self.offset_y.get(),
                    // We substracted the margin from the text area before, so
                    // we have to add that back now
                    margin: props.margins.right,
                    ..Default::default()
                },
                text_area,
            );
        }
        if has_horizontal_scroll {
            frame.render_widget(
                Scrollbar {
                    content_length: self.text_width.get(),
                    offset: self.offset_x.get(),
                    orientation: ScrollbarOrientation::HorizontalBottom,
                    // See note on other scrollbar for +1
                    margin: props.margins.bottom,
                },
                text_area,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        test_util::{harness, terminal, TestHarness, TestTerminal},
        view::test_util::TestComponent,
    };
    use crossterm::event::{KeyCode, KeyModifiers};
    use ratatui::text::Span;
    use rstest::rstest;

    #[rstest]
    fn test_scroll(
        #[with(10, 4)] terminal: TestTerminal,
        harness: TestHarness,
    ) {
        let text =
            Text::from("line 1\nline 2 is longer\nline 3\nline 4\nline 5");
        let mut component = TestComponent::new(
            &harness,
            &terminal,
            TextWindow::default(),
            TextWindowProps {
                text: &text,
                // Don't overflow the frame
                margins: ScrollbarMargins {
                    right: 0,
                    bottom: 0,
                },
                footer: None,
            },
        );
        terminal.assert_buffer_lines([
            vec![line_num(1), " line 1 ▲".into()],
            vec![line_num(2), " line 2 █".into()],
            vec![line_num(3), " line 3 █".into()],
            vec![line_num(4), " ◀■■■═══▶".into()],
        ]);

        // Scroll down
        component.send_key(KeyCode::Down).assert_empty();
        terminal.assert_buffer_lines([
            vec![line_num(2), " line 2 ▲".into()],
            vec![line_num(3), " line 3 █".into()],
            vec![line_num(4), " line 4 █".into()],
            vec![line_num(5), " ◀■■■═══▶".into()],
        ]);

        // Scroll back up
        component.send_key(KeyCode::Up).assert_empty();
        component.send_key(KeyCode::Up).assert_empty(); // Does nothing
        terminal.assert_buffer_lines([
            vec![line_num(1), " line 1 ▲".into()],
            vec![line_num(2), " line 2 █".into()],
            vec![line_num(3), " line 3 █".into()],
            vec![line_num(4), " ◀■■■═══▶".into()],
        ]);

        // Scroll right
        component
            .send_key_modifiers(KeyCode::Right, KeyModifiers::SHIFT)
            .assert_empty();
        component
            .send_key_modifiers(KeyCode::Right, KeyModifiers::SHIFT)
            .assert_empty();
        component
            .send_key_modifiers(KeyCode::Right, KeyModifiers::SHIFT)
            .assert_empty();
        terminal.assert_buffer_lines([
            vec![line_num(1), " e 1    ▲".into()],
            vec![line_num(2), " e 2 is █".into()],
            vec![line_num(3), " e 3    █".into()],
            vec![line_num(4), " ◀═■■■══▶".into()],
        ]);

        // Scroll back left
        component
            .send_key_modifiers(KeyCode::Left, KeyModifiers::SHIFT)
            .assert_empty();
        component
            .send_key_modifiers(KeyCode::Left, KeyModifiers::SHIFT)
            .assert_empty();
        component
            .send_key_modifiers(KeyCode::Left, KeyModifiers::SHIFT)
            .assert_empty();
        component
            .send_key_modifiers(KeyCode::Left, KeyModifiers::SHIFT)
            .assert_empty(); // Does nothing
        terminal.assert_buffer_lines([
            vec![line_num(1), " line 1 ▲".into()],
            vec![line_num(2), " line 2 █".into()],
            vec![line_num(3), " line 3 █".into()],
            vec![line_num(4), " ◀■■■═══▶".into()],
        ]);
    }

    #[rstest]
    fn test_unicode(
        #[with(35, 3)] terminal: TestTerminal,
        harness: TestHarness,
    ) {
        let text = Text::from("intro\n💚💙💜 this is a longer line\noutro");
        TestComponent::new(
            &harness,
            &terminal,
            TextWindow::default(),
            TextWindowProps {
                text: &text,
                // Don't overflow the frame
                margins: ScrollbarMargins {
                    right: 0,
                    bottom: 0,
                },
                footer: None,
            },
        );
        terminal.assert_buffer_lines([
            vec![line_num(1), " intro                            ".into()],
            vec![line_num(2), " 💚💙💜 this is a longer line    ".into()],
            vec![line_num(3), " outro                            ".into()],
        ]);
    }

    #[rstest]
    fn test_unicode_scroll(
        #[with(10, 2)] terminal: TestTerminal,
        harness: TestHarness,
    ) {
        let text = Text::from("💚💙💜💚💙💜");
        TestComponent::new(
            &harness,
            &terminal,
            TextWindow::default(),
            TextWindowProps {
                text: &text,
                // Don't overflow the frame
                margins: ScrollbarMargins {
                    right: 0,
                    bottom: 0,
                },
                footer: None,
            },
        );
        terminal.assert_buffer_lines([
            vec![line_num(1), " 💚💙💜💚".into()],
            vec![line_num(0), " ◀■■■■══▶".into()],
        ]);
    }

    /// Shrinking text reduces the maximum scroll. Scroll state should
    /// automatically be clamped to match
    #[rstest]
    fn test_shrink_text(
        #[with(10, 3)] terminal: TestTerminal,
        harness: TestHarness,
    ) {
        let text = ["1 this is a long line", "2", "3", "4", "5"]
            .join("\n")
            .into();
        let mut component = TestComponent::new(
            &harness,
            &terminal,
            TextWindow::default(),
            TextWindowProps {
                text: &text,
                // Don't overflow the frame
                margins: ScrollbarMargins {
                    right: 0,
                    bottom: 0,
                },
                footer: None,
            },
        );

        // Scroll out a bit
        component.data_mut().scroll_down(2);
        component.data_mut().scroll_right(10);
        assert_eq!(component.data().offset_x.get(), 10);
        assert_eq!(component.data().offset_y.get(), 2);

        let text = ["1 less long line", "2", "3", "4"].join("\n").into();
        component.set_props(TextWindowProps {
            text: &text,
            margins: ScrollbarMargins {
                right: 0,
                bottom: 0,
            },
            footer: None,
        });
        component.drain_draw().assert_empty();

        assert_eq!(component.data().offset_x.get(), 8);
        assert_eq!(component.data().offset_y.get(), 1);
    }

    /// Growing the window reduces the maximum scroll. Scroll state should
    /// automatically be clamped to match
    #[rstest]
    fn test_grow_window(terminal: TestTerminal, harness: TestHarness) {
        let text = ["1 this is a long line", "2", "3", "4", "5"]
            .join("\n")
            .into();
        let mut component = TestComponent::new(
            &harness,
            &terminal,
            TextWindow::default(),
            TextWindowProps {
                text: &text,
                // Don't overflow the frame
                margins: ScrollbarMargins {
                    right: 0,
                    bottom: 0,
                },
                footer: None,
            },
        );

        component.set_area(Rect::new(0, 0, 10, 3));
        component.drain_draw().assert_empty();

        // Scroll out a bit
        component.data_mut().scroll_down(2);
        component.data_mut().scroll_right(10);
        assert_eq!(component.data().offset_x.get(), 10);
        assert_eq!(component.data().offset_y.get(), 2);

        component.set_area(Rect::new(0, 0, 15, 4));
        component.drain_draw().assert_empty();

        assert_eq!(component.data().offset_x.get(), 8);
        assert_eq!(component.data().offset_y.get(), 1);
    }

    /// Style some text as gutter line numbers
    fn line_num(n: u16) -> Span<'static> {
        let s = if n > 0 { n.to_string() } else { " ".into() };
        Span::styled(s, TuiContext::get().styles.text_window.gutter)
    }
}