use crate::{
	app::Environment,
	components::{
		visibility_blocking, CommandBlocking, CommandInfo, Component,
		DrawableComponent, EventState,
	},
	keys::{key_match, SharedKeyConfig},
	queue::{Action, InternalEvent, Queue},
	strings, ui,
};
use anyhow::Result;
use crossterm::event::Event;
use ratatui::{layout::Rect, text::Text, widgets::Clear, Frame};
use std::borrow::Cow;
use ui::style::SharedTheme;

use super::render_popup;

///
pub struct ConfirmPopup {
	target: Option<Action>,
	visible: bool,
	queue: Queue,
	theme: SharedTheme,
	key_config: SharedKeyConfig,
}

impl DrawableComponent for ConfirmPopup {
	fn draw(&self, f: &mut Frame, _rect: Rect) -> Result<()> {
		if self.visible {
			let (title, msg) = self.get_text();

			let txt = Text::styled(
				Cow::from(msg),
				self.theme.text_danger(),
			);

			let area = ui::centered_rect(50, 20, f.area());
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
			render_popup(
				f.buffer_mut(),
				area,
				&title,
				txt,
				&self.theme,
				true,
				true,
			);
		}

		Ok(())
	}
}

impl Component for ConfirmPopup {
	fn commands(
		&self,
		out: &mut Vec<CommandInfo>,
		_force_all: bool,
	) -> CommandBlocking {
		out.push(CommandInfo::new(
			strings::commands::confirm_action(&self.key_config),
			true,
			self.visible,
		));
		out.push(CommandInfo::new(
			strings::commands::close_popup(&self.key_config),
			true,
			self.visible,
		));

		visibility_blocking(self)
	}

	fn event(&mut self, ev: &Event) -> Result<EventState> {
		if self.visible {
			if let Event::Key(e) = ev {
				if key_match(e, self.key_config.keys.exit_popup) {
					self.hide();
				} else if key_match(e, self.key_config.keys.enter) {
					self.confirm();
				} else if key_match(e, self.key_config.keys.copy) {
					// Allow 'y' (copy key) to confirm as well
					self.confirm();
				}

				return Ok(EventState::Consumed);
			}
		}

		Ok(EventState::NotConsumed)
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

impl ConfirmPopup {
	///
	pub fn new(env: &Environment) -> Self {
		Self {
			target: None,
			visible: false,
			queue: env.queue.clone(),
			theme: env.theme.clone(),
			key_config: env.key_config.clone(),
		}
	}
	///
	pub fn open(&mut self, a: Action) -> Result<()> {
		self.target = Some(a);
		self.show()?;

		Ok(())
	}
	///
	pub fn confirm(&mut self) {
		if let Some(a) = self.target.take() {
			self.queue.push(InternalEvent::ConfirmedAction(a));
		}

		self.hide();
	}

	#[allow(clippy::too_many_lines)]
	fn get_text(&self) -> (String, String) {
		if let Some(ref a) = self.target {
			return match a {
				Action::Reset(_) => (
					strings::confirm_title_reset(),
					strings::confirm_msg_reset(),
				),
				Action::StashDrop(ids) => (
					strings::confirm_title_stashdrop(
						&self.key_config,
						ids.len() > 1,
					),
					strings::confirm_msg_stashdrop(
						&self.key_config,
						ids,
					),
				),
				Action::StashPop(_) => (
					strings::confirm_title_stashpop(&self.key_config),
					strings::confirm_msg_stashpop(&self.key_config),
				),
				Action::StashApply(_) => (
					strings::confirm_title_stashapply(
						&self.key_config,
					),
					strings::confirm_msg_stashapply(&self.key_config),
				),
				Action::ResetHunk(_, _) => (
					strings::confirm_title_reset(),
					strings::confirm_msg_resethunk(&self.key_config),
				),
				Action::ResetLines(_, lines) => (
					strings::confirm_title_reset(),
					strings::confirm_msg_reset_lines(lines.len()),
				),
				Action::DeleteLocalBranch(branch_ref) => (
					strings::confirm_title_delete_branch(
						&self.key_config,
					),
					strings::confirm_msg_delete_branch(
						&self.key_config,
						branch_ref,
					),
				),
				Action::DeleteRemoteBranch(branch_ref) => (
					strings::confirm_title_delete_remote_branch(
						&self.key_config,
					),
					strings::confirm_msg_delete_remote_branch(
						&self.key_config,
						branch_ref,
					),
				),
				Action::DeleteRemote(remote_name) => (
					strings::confirm_title_delete_remote(
						&self.key_config,
					),
					strings::confirm_msg_delete_remote(
						&self.key_config,
						remote_name,
					),
				),
				Action::DeleteTag(tag_name) => (
					strings::confirm_title_delete_tag(
						&self.key_config,
					),
					strings::confirm_msg_delete_tag(
						&self.key_config,
						tag_name,
					),
				),
				Action::DeleteRemoteTag(_tag_name, remote) => (
					strings::confirm_title_delete_tag_remote(),
					strings::confirm_msg_delete_tag_remote(remote),
				),
				Action::ForcePush(branch, force) => {
					let branch_name = branch
                        .rsplit('/')
                        .next()
                        .expect("There was no / in the head reference which is impossible in git");
					if *force {
						(
							strings::confirm_title_force_push(
								&self.key_config,
							),
							strings::confirm_msg_force_push(
								&self.key_config,
								branch_name,
							),
						)
					} else {
						(
							strings::confirm_title_push(),
							strings::confirm_msg_push(branch_name),
						)
					}
				}
				Action::PullMerge { incoming, rebase } => (
					strings::confirm_title_merge(
						&self.key_config,
						*rebase,
					),
					strings::confirm_msg_merge(
						&self.key_config,
						*incoming,
						*rebase,
					),
				),
				Action::AbortMerge => (
					strings::confirm_title_abortmerge(),
					strings::confirm_msg_revertchanges(),
				),
				Action::AbortRebase => (
					strings::confirm_title_abortrebase(),
					strings::confirm_msg_abortrebase(),
				),
				Action::AbortRevert => (
					strings::confirm_title_abortrevert(),
					strings::confirm_msg_revertchanges(),
				),
				Action::UndoCommit => (
					strings::confirm_title_undo_commit(),
					strings::confirm_msg_undo_commit(),
				),
				Action::PushTags => (
					strings::confirm_title_push_tags(),
					strings::confirm_msg_push_tags(),
				),
			};
		}

		(String::new(), String::new())
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::ui::style::Theme;
	use ratatui::{
		buffer::Buffer,
		widgets::{Clear, Widget},
	};

	/// Simulate the real failure mode: a wide (CJK) grapheme sits just
	/// outside the popup area (in the diff background). Without clearing a
	/// 1-cell margin, that grapheme visually overflows into the popup's
	/// left border cell. With the margin clear, the border is preserved.
	#[test]
	fn popup_left_border_survives_adjacent_wide_char() {
		let theme = Theme::default();
		let mut buf = Buffer::empty(Rect::new(0, 0, 30, 7));

		// Fill the row y=3 (popup's middle row) with CJK chars, including
		// the cell immediately left of the popup area.
		for x in 0..30 {
			buf[(x, 3)].set_symbol("中");
		}

		// Popup area: x=10..20, y=2..5 (width 10, height 3)
		let area = Rect::new(10, 2, 10, 3);

		// Clear a 1-cell margin around the popup (mirrors draw()).
		let clear_area = Rect::new(
			area.x.saturating_sub(1),
			area.y.saturating_sub(1),
			area.width.saturating_add(2),
			area.height.saturating_add(2),
		);
		Clear.render(clear_area, &mut buf);

		// Render the popup block + text.
		let txt =
			Text::styled("confirm reset hunk?", theme.text_danger());
		super::render_popup(
			&mut buf, area, "Reset", txt, &theme, true, true,
		);

		// The left border cell at (10, 3) must be a vertical border
		// glyph, not a CJK char that overflowed from (9, 3).
		assert_eq!(
			buf[(10, 3)].symbol(),
			"┃",
			"left border must not be stomped by adjacent CJK"
		);
		// Also check the cell just outside (9, 3) was cleared so the
		// CJK char there cannot visually extend into the border.
		assert_eq!(
			buf[(9, 3)].symbol(),
			" ",
			"cell adjacent to popup must be cleared"
		);
	}
}
