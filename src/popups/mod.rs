mod blame_file;
mod branchlist;
mod checkout_option;
mod commit;
mod compare_commits;
mod confirm;
mod copy_path;
mod create_branch;
mod create_remote;
mod externaleditor;
mod fetch;
mod file_revlog;
mod fuzzy_find;
mod goto_line;
mod help;
mod inspect_commit;
mod log_search;
mod msg;
mod options;
mod pull;
mod push;
mod push_tags;
mod remotelist;
mod rename_branch;
mod rename_remote;
mod reset;
mod revision_files;
mod stashmsg;
mod submodules;
mod tag_commit;
mod taglist;
mod update_remote_url;

pub use blame_file::{BlameFileOpen, BlameFilePopup};
pub use branchlist::BranchListPopup;
pub use checkout_option::CheckoutOptionPopup;
pub use commit::CommitPopup;
pub use compare_commits::CompareCommitsPopup;
pub use confirm::ConfirmPopup;
pub use copy_path::CopyPathPopup;
pub use create_branch::CreateBranchPopup;
pub use create_remote::CreateRemotePopup;
pub use externaleditor::ExternalEditorPopup;
pub use fetch::FetchPopup;
pub use file_revlog::{FileRevOpen, FileRevlogPopup};
pub use fuzzy_find::FuzzyFindPopup;
pub use goto_line::GotoLinePopup;
pub use help::HelpPopup;
pub use inspect_commit::{InspectCommitOpen, InspectCommitPopup};
pub use log_search::LogSearchPopupPopup;
pub use msg::MsgPopup;
pub use options::{AppOption, OptionsPopup};
pub use pull::PullPopup;
pub use push::PushPopup;
pub use push_tags::PushTagsPopup;
pub use remotelist::RemoteListPopup;
pub use rename_branch::RenameBranchPopup;
pub use rename_remote::RenameRemotePopup;
pub use reset::ResetPopup;
pub use revision_files::{FileTreeOpen, RevisionFilesPopup};
pub use stashmsg::StashMsgPopup;
pub use submodules::SubmodulesListPopup;
pub use tag_commit::TagCommitPopup;
pub use taglist::TagListPopup;
pub use update_remote_url::UpdateRemoteUrlPopup;

use crate::ui::style::Theme;
use ratatui::{
	buffer::Buffer,
	layout::Rect,
	text::{Line, Span, Text},
	widgets::{Block, BorderType, Borders, Padding, Widget},
};

/// Render the popup block (border + title) and its text content directly into
/// the buffer.
///
/// We avoid `Paragraph` here because its `render_text` path writes a wide
/// (CJK/emoji) grapheme with `Cell::set_symbol` but does not reset the
/// trailing cell it occupies. The crossterm backend then prints the next
/// cell's symbol at the advanced cursor position, overwriting the right
/// border. `Buffer::set_stringn` resets the trailing cell, so we use it
/// instead. Long lines are word-wrapped beforehand using `unicode-width` so
/// CJK content still wraps instead of being silently truncated.
fn render_popup<'a, T>(
	buf: &mut Buffer,
	area: Rect,
	title: &'a str,
	content: T,
	theme: &Theme,
	focused: bool,
	block: bool,
) where
	T: Into<Text<'a>>,
{
	let block = if block {
		Block::default()
			.title(Span::styled(title, theme.title(focused)))
			.borders(Borders::ALL)
			.border_type(BorderType::Thick)
			.border_style(theme.block(focused))
			.padding(Padding::horizontal(1))
	} else {
		Block::default()
	};

	// Compute the inner area before consuming `block` in `render`.
	let inner = block.inner(area);
	block.render(area, buf);

	if inner.is_empty() {
		return;
	}

	let style = theme.text_danger();
	let text: Text<'a> = content.into();
	let max_width = usize::from(inner.width);

	for (i, line) in text.lines.iter().enumerate() {
		let Ok(row) = u16::try_from(i) else {
			break;
		};
		let y = inner.top().saturating_add(row);
		if y >= inner.bottom() {
			break;
		}

		let line_style = line.style.patch(style);
		let wrapped = wrap_line(line, max_width);
		for (j, chunk) in wrapped.iter().enumerate() {
			let Ok(sub_row) = u16::try_from(j) else {
				break;
			};
			let yy = y.saturating_add(sub_row);
			if yy >= inner.bottom() {
				break;
			}
			let styled_chunk =
				Line::from(chunk.iter().cloned().collect::<Vec<_>>())
					.style(line_style);
			buf.set_line(
				inner.left(),
				yy,
				&styled_chunk,
				inner.width,
			);
		}
	}
}

/// Word-wrap a single `Line` into multiple `Line`s each fitting within
/// `max_width` display cells. Wide graphemes (CJK, emoji) that do not fit at
/// the end of a line are moved to the next line; if a single grapheme is
/// wider than `max_width`, it is dropped (mirroring `Buffer::set_stringn`).
///
/// Note: `components::async_delta::wrap_line` does a similar job but iterates
/// by `char` instead of grapheme. We keep this separate copy because
/// grapheme-based splitting is correct for ZWJ emoji sequences (which a
/// `char` walk would split mid-cluster); popups render short user-facing
/// strings where that distinction matters more than the diff path's
/// reuse-coverage.
fn wrap_line<'a>(
	line: &'a Line<'a>,
	max_width: usize,
) -> Vec<Line<'a>> {
	if max_width == 0 {
		return vec![line.clone()];
	}
	let mut out: Vec<Line<'a>> = Vec::new();
	let mut current_spans: Vec<Span<'a>> = Vec::new();
	let mut current_width: usize = 0;

	for span in &line.spans {
		let mut buf = String::new();
		for grapheme in
			unicode_segmentation::UnicodeSegmentation::graphemes(
				span.content.as_ref(),
				true,
			) {
			let w = unicode_width::UnicodeWidthStr::width(grapheme);
			if w == 0 {
				buf.push_str(grapheme);
				continue;
			}
			if w > max_width {
				// too wide to ever fit; flush current then skip
				if !buf.is_empty() {
					current_spans.push(Span::styled(
						std::mem::take(&mut buf),
						span.style,
					));
				}
				if !current_spans.is_empty() {
					flush_span(
						&mut current_spans,
						&mut current_width,
						&mut out,
						line.style,
					);
				}
				continue;
			}
			if current_width + w > max_width {
				if !buf.is_empty() {
					current_spans.push(Span::styled(
						std::mem::take(&mut buf),
						span.style,
					));
				}
				flush_span(
					&mut current_spans,
					&mut current_width,
					&mut out,
					line.style,
				);
			}
			buf.push_str(grapheme);
			current_width += w;
		}
		if !buf.is_empty() {
			current_spans.push(Span::styled(
				std::mem::take(&mut buf),
				span.style,
			));
		}
	}

	out.push(Line::from(current_spans).style(line.style));
	if out.is_empty() {
		out.push(Line::default().style(line.style));
	}
	out
}

fn flush_span<'a>(
	current_spans: &mut Vec<Span<'a>>,
	current_width: &mut usize,
	out: &mut Vec<Line<'a>>,
	line_style: ratatui::style::Style,
) {
	let spans = std::mem::take(current_spans);
	out.push(Line::from(spans).style(line_style));
	*current_width = 0;
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn wrap_line_keeps_ascii_together_when_it_fits() {
		let line = Line::from("confirm reset hunk?");
		let wrapped = wrap_line(&line, 50);
		assert_eq!(wrapped.len(), 1);
		assert_eq!(wrapped[0].width(), 19);
	}

	#[test]
	fn wrap_line_wraps_ascii_when_too_long() {
		let line = Line::from("confirm reset hunk?");
		let wrapped = wrap_line(&line, 10);
		assert!(wrapped.len() > 1);
		// each wrapped line fits within the limit
		for w in &wrapped {
			assert!(w.width() <= 10);
		}
	}

	#[test]
	fn wrap_line_moves_wide_char_to_next_line_instead_of_overflowing()
	{
		// 8 ASCII + 1 CJK (width 2) = 10 cells; width limit 9 must not
		// place the CJK at the last cell (which would stomp the border).
		let line = Line::from("12345678中");
		let wrapped = wrap_line(&line, 9);
		assert_eq!(wrapped.len(), 2);
		assert_eq!(wrapped[0].width(), 8);
		assert_eq!(wrapped[1].width(), 2);
	}

	#[test]
	fn wrap_line_keeps_wide_char_when_it_fits_exactly() {
		let line = Line::from("123456中");
		let wrapped = wrap_line(&line, 8);
		assert_eq!(wrapped.len(), 1);
		assert_eq!(wrapped[0].width(), 8);
	}

	#[test]
	fn wrap_line_drops_grapheme_wider_than_limit() {
		let line = Line::from("中");
		let wrapped = wrap_line(&line, 1);
		// width-2 grapheme cannot fit in 1 cell; it is dropped
		assert_eq!(wrapped.len(), 1);
		assert_eq!(wrapped[0].width(), 0);
	}

	#[test]
	fn render_popup_draws_thick_border_around_content() {
		use crate::ui::style::Theme;
		use ratatui::buffer::Buffer;

		let theme = Theme::default();
		let mut buf = Buffer::empty(Rect::new(0, 0, 30, 5));

		// content with a CJK char to exercise wide-char handling
		let txt = Text::styled("确认重置?", theme.text_danger());
		render_popup(
			&mut buf,
			Rect::new(0, 0, 30, 5),
			"Reset",
			txt,
			&theme,
			true,
			true,
		);

		// four corners should be thick border glyphs
		assert_eq!(buf[(0, 0)].symbol(), "┏", "top-left corner");
		assert_eq!(buf[(29, 0)].symbol(), "┓", "top-right corner");
		assert_eq!(buf[(0, 4)].symbol(), "┗", "bottom-left corner");
		assert_eq!(buf[(29, 4)].symbol(), "┛", "bottom-right corner");
		// right edge should be a vertical border, not stomped by content
		assert_eq!(buf[(29, 2)].symbol(), "┃", "right edge mid");
		// left edge should be a vertical border, not stomped by content
		assert_eq!(buf[(0, 2)].symbol(), "┃", "left edge mid");
	}

	#[test]
	fn render_popup_wide_char_at_start_does_not_stomp_left_border() {
		use crate::ui::style::Theme;
		use ratatui::buffer::Buffer;

		let theme = Theme::default();
		// narrow popup (width 6): inner text area is 6 - 2 (border) - 2
		// (padding) = 2 cells. Content "中" (width 2) fits exactly.
		let mut buf = Buffer::empty(Rect::new(0, 0, 6, 3));
		let txt = Text::styled("中", theme.text_danger());
		render_popup(
			&mut buf,
			Rect::new(0, 0, 6, 3),
			"R",
			txt,
			&theme,
			true,
			true,
		);
		// left border at (0, 1) must remain the vertical border glyph
		assert_eq!(
			buf[(0, 1)].symbol(),
			"┃",
			"left border preserved"
		);
		// the wide char lands at (2, 1) — inside the padding-inner area
		assert_eq!(
			buf[(2, 1)].symbol(),
			"中",
			"wide char placed in inner area"
		);
		// right border at (5, 1) must remain the vertical border glyph
		assert_eq!(
			buf[(5, 1)].symbol(),
			"┃",
			"right border preserved"
		);
	}

	#[test]
	fn render_popup_preserves_border_with_background_noise() {
		use crate::ui::style::Theme;
		use ratatui::buffer::Buffer;

		// Simulate a fullscreen diff background: fill the buffer with
		// CJK content + colored styles, then render the popup on top.
		let theme = Theme::default();
		let mut buf = Buffer::empty(Rect::new(0, 0, 40, 10));
		// fill with CJK chars to simulate diff content behind the popup
		for y in 0..10 {
			for x in 0..40 {
				buf[(x, y)].set_symbol("中");
			}
		}
		// Clear + render popup in the center
		let area = Rect::new(10, 3, 20, 4);
		for y in area.top()..area.bottom() {
			for x in area.left()..area.right() {
				buf[(x, y)].reset();
			}
		}
		let txt =
			Text::styled("确认重置此代码块?", theme.text_danger());
		render_popup(
			&mut buf, area, "Reset", txt, &theme, true, true,
		);
		// all four corners of the popup must be thick border glyphs
		assert_eq!(buf[(10, 3)].symbol(), "┏", "tl");
		assert_eq!(buf[(29, 3)].symbol(), "┓", "tr");
		assert_eq!(buf[(10, 6)].symbol(), "┗", "bl");
		assert_eq!(buf[(29, 6)].symbol(), "┛", "br");
		// left/right edges mid-popup must be vertical borders
		assert_eq!(buf[(10, 4)].symbol(), "┃", "left edge");
		assert_eq!(buf[(29, 4)].symbol(), "┃", "right edge");
	}
}
