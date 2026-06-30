use crate::components::{
	visibility_blocking, CommandBlocking, CommandInfo, Component,
	DrawableComponent, EventState, ScrollType, VerticalScroll,
};
use crate::strings::order;
use crate::{
	app::Environment,
	keys::{key_match, SharedKeyConfig},
	strings, ui,
};
use anyhow::Result;
use crossterm::event::Event;
use ratatui::text::Line;
use ratatui::{
	layout::Rect,
	text::Span,
	widgets::{Block, BorderType, Borders, Clear},
	Frame,
};
use ui::style::SharedTheme;

pub struct MsgPopup {
	title: String,
	msg: String,
	visible: bool,
	theme: SharedTheme,
	key_config: SharedKeyConfig,
	scroll: VerticalScroll,
}

const POPUP_HEIGHT: u16 = 25;
const BORDER_WIDTH: u16 = 2;
const MINIMUM_WIDTH: u16 = 60;

impl DrawableComponent for MsgPopup {
	fn draw(&self, f: &mut Frame, _rect: Rect) -> Result<()> {
		if !self.visible {
			return Ok(());
		}

		let max_width = f.area().width.max(MINIMUM_WIDTH);

		// determine the maximum width of text block
		let width = self
			.msg
			.lines()
			.map(str::len)
			.max()
			.unwrap_or(0)
			.saturating_add(BORDER_WIDTH.into())
			.clamp(MINIMUM_WIDTH.into(), max_width.into())
			.try_into()
			.expect("can't fail because we're clamping to u16 value");

		let area =
			ui::centered_rect_absolute(width, POPUP_HEIGHT, f.area());

		// Wrap lines and break words if there is not enough space
		let wrapped_msg = bwrap::wrap_maybrk!(
			&self.msg,
			area.width.saturating_sub(BORDER_WIDTH).into()
		);

		let msg_lines: Vec<String> =
			wrapped_msg.lines().map(String::from).collect();
		let line_num = msg_lines.len();

		let height = POPUP_HEIGHT
			.saturating_sub(BORDER_WIDTH)
			.min(f.area().height.saturating_sub(BORDER_WIDTH));

		let top =
			self.scroll.update_no_selection(line_num, height.into());

		let scrolled_lines = msg_lines
			.iter()
			.skip(top)
			.take(height.into())
			.map(|line| {
				Line::from(vec![Span::styled(
					line.clone(),
					self.theme.text(true, false),
				)])
			})
			.collect::<Vec<Line>>();

		// Clear a 1-cell margin around the popup so that a wide
		// (CJK/emoji) grapheme sitting just outside the popup area
		// cannot visually overflow into the popup's border cells.
		let clear_area = Rect::new(
			area.x.saturating_sub(1),
			area.y.saturating_sub(1),
			area.width.saturating_add(2),
			area.height.saturating_add(2),
		);
		f.render_widget(Clear, clear_area);

		let block = Block::default()
			.title(Span::styled(
				self.title.as_str(),
				self.theme.text_danger(),
			))
			.borders(Borders::ALL)
			.border_type(BorderType::Thick);
		ui::render_block_text(
			f.buffer_mut(),
			area,
			block,
			&scrolled_lines,
		);

		self.scroll.draw(f, area, &self.theme);

		Ok(())
	}
}

impl Component for MsgPopup {
	fn commands(
		&self,
		out: &mut Vec<CommandInfo>,
		_force_all: bool,
	) -> CommandBlocking {
		out.push(CommandInfo::new(
			strings::commands::close_popup(&self.key_config),
			true,
			self.visible,
		));
		out.push(
			CommandInfo::new(
				strings::commands::navigate_commit_message(
					&self.key_config,
				),
				true,
				self.visible,
			)
			.order(order::NAV),
		);

		visibility_blocking(self)
	}

	fn event(&mut self, ev: &Event) -> Result<EventState> {
		if self.visible {
			if let Event::Key(e) = ev {
				if key_match(e, self.key_config.keys.exit_popup) {
					self.hide();
				} else if key_match(e, self.key_config.keys.move_down)
					|| key_match(e, self.key_config.keys.popup_down)
				{
					self.scroll.move_top(ScrollType::Down);
				} else if key_match(e, self.key_config.keys.move_up)
					|| key_match(e, self.key_config.keys.popup_up)
				{
					self.scroll.move_top(ScrollType::Up);
				}
			}
			Ok(EventState::Consumed)
		} else {
			Ok(EventState::NotConsumed)
		}
	}

	fn is_visible(&self) -> bool {
		self.visible
	}

	fn hide(&mut self) {
		self.visible = false;
	}

	fn show(&mut self) -> Result<()> {
		self.visible = true;

		Ok(())
	}
}

impl MsgPopup {
	pub fn new(env: &Environment) -> Self {
		Self {
			title: String::new(),
			msg: String::new(),
			visible: false,
			theme: env.theme.clone(),
			key_config: env.key_config.clone(),
			scroll: VerticalScroll::new(),
		}
	}

	fn set_new_msg(
		&mut self,
		msg: &str,
		title: String,
	) -> Result<()> {
		self.title = title;
		self.msg = msg.to_string();
		self.scroll.reset();
		self.show()
	}

	///
	pub fn show_error(&mut self, msg: &str) -> Result<()> {
		self.set_new_msg(
			msg,
			strings::msg_title_error(&self.key_config),
		)
	}

	///
	pub fn show_info(&mut self, msg: &str) -> Result<()> {
		self.set_new_msg(
			msg,
			strings::msg_title_info(&self.key_config),
		)
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::app::Environment;
	use ratatui::{backend::TestBackend, Terminal};

	/// Reproduces the wide-char border overflow on `MsgPopup`: when the
	/// buffer behind the popup is filled with CJK chars (e.g. a fullscreen
	/// diff background), a wide grapheme sitting just outside the popup
	/// visually overflows into the popup's left border cell. The fix clears
	/// a 1-cell margin around the popup, so the border is preserved.
	#[test]
	fn msg_popup_left_border_survives_adjacent_wide_char() {
		let env = Environment::test_env();
		let mut popup = MsgPopup::new(&env);
		// CJK info message — exercises wide-char rendering inside the popup.
		popup
			.show_info("复制完成：测试中文路径/文件.txt")
			.expect("show_info");

		// Terminal size chosen so the popup lands at a known position:
		// POPUP_HEIGHT=25, MINIMUM_WIDTH=60, message line is short so
		// width=60. centered_rect_absolute => x=(100-60)/2=20,
		// y=(30-25)/2=2, size 60x25.
		let backend = TestBackend::new(100, 30);
		let mut terminal =
			Terminal::new(backend).expect("create terminal");

		terminal
			.draw(|f| {
				// Fill the frame buffer with CJK chars to simulate a
				// fullscreen diff background behind the popup.
				for y in 0..f.area().height {
					for x in 0..f.area().width {
						f.buffer_mut()[(x, y)].set_symbol("中");
					}
				}
				popup.draw(f, f.area()).expect("draw failed");
			})
			.expect("terminal draw");

		let buf = terminal.backend().buffer();
		// Popup area is x=20..80, y=2..27. Check the left border at the
		// popup's vertical middle (y=14): it must be the thick vertical
		// border glyph, not a CJK char that overflowed from x=19.
		assert_eq!(
			buf[(20, 14)].symbol(),
			"┃",
			"left border must not be stomped by adjacent CJK"
		);
		// The cell just outside the popup (x=19) must be cleared so the
		// CJK char there cannot visually extend into the border.
		assert_eq!(
			buf[(19, 14)].symbol(),
			" ",
			"cell adjacent to popup must be cleared"
		);
		// Right border must also survive.
		assert_eq!(
			buf[(79, 14)].symbol(),
			"┃",
			"right border must not be stomped by CJK content"
		);
	}
}
