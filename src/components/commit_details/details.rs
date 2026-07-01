use crate::{
	app::Environment,
	components::{
		commit_details::style::style_detail,
		dialog_paragraph,
		utils::{scroll_vertical::VerticalScroll, time_to_string},
		CommandBlocking, CommandInfo, Component, DrawableComponent,
		EventState, ScrollType,
	},
	keys::{key_match, SharedKeyConfig},
	queue::{InternalEvent, Queue},
	strings::{self, order},
	ui::style::SharedTheme,
};
use anyhow::Result;
use asyncgit::sync::{
	self, CommitDetails, CommitId, CommitMessage, RepoPathRef, Tag,
};
use crossterm::event::Event;
use ratatui::{
	layout::{Constraint, Direction, Layout, Rect},
	style::{Modifier, Style},
	text::{Line, Span, Text},
	Frame,
};
use std::{borrow::Cow, cell::Cell};
use sync::CommitTags;

use super::style::Detail;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum DetailsFocus {
	Info,
	Message,
}

pub struct DetailsComponent {
	repo: RepoPathRef,
	data: Option<CommitDetails>,
	tags: Vec<Tag>,
	theme: SharedTheme,
	/// Whether this component is the focused sub-component of the
	/// parent (`CommitDetailsComponent`). Distinct from `focus`,
	/// which tracks *which* of Info/Message is focused within.
	details_focused: bool,
	focus: DetailsFocus,
	current_width: Cell<u16>,
	scroll: VerticalScroll,
	info_scroll: VerticalScroll,
	scroll_to_bottom_next_draw: Cell<bool>,
	key_config: SharedKeyConfig,
	queue: Queue,
}

type WrappedCommitMessage<'a> =
	(Vec<Cow<'a, str>>, Vec<Cow<'a, str>>);

impl DetailsComponent {
	///
	pub fn new(env: &Environment, focused: bool) -> Self {
		Self {
			repo: env.repo.clone(),
			data: None,
			tags: Vec::new(),
			theme: env.theme.clone(),
			details_focused: focused,
			focus: DetailsFocus::Message,
			scroll_to_bottom_next_draw: Cell::new(false),
			current_width: Cell::new(0),
			scroll: VerticalScroll::new(),
			info_scroll: VerticalScroll::new(),
			key_config: env.key_config.clone(),
			queue: env.queue.clone(),
		}
	}

	fn copy_message(&self) {
		if let Some(data) = &self.data {
			if let Some(message) = &data.message {
				let text = message.clone().combine();
				if crate::clipboard::copy_string(&text).is_err() {
					self.queue.push(InternalEvent::ShowErrorMsg(
						strings::POPUP_FAIL_COPY.to_string(),
					));
				} else {
					self.queue.push(
						InternalEvent::ShowInfoLabeledMsg(
							strings::POPUP_SUCCESS_COPY.to_string(),
							strings::copy_success(&text),
						),
					);
				}
			}
		}
	}

	fn copy_info(&self) {
		if self.data.is_some() {
			let lines = self.get_text_info();
			let text = lines
				.iter()
				.map(|line| {
					line.spans
						.iter()
						.map(|span| span.content.as_ref())
						.collect::<String>()
				})
				.collect::<Vec<_>>()
				.join("\n");
			if crate::clipboard::copy_string(&text).is_err() {
				self.queue.push(InternalEvent::ShowErrorMsg(
					strings::POPUP_FAIL_COPY.to_string(),
				));
			} else {
				self.queue.push(InternalEvent::ShowInfoLabeledMsg(
					strings::POPUP_SUCCESS_COPY.to_string(),
					strings::copy_success(&text),
				));
			}
		}
	}

	pub fn set_focus(&mut self, focus: DetailsFocus) {
		match focus {
			DetailsFocus::Info => {
				self.info_scroll.reset();
			}
			DetailsFocus::Message => {
				self.scroll_to_bottom_next_draw.set(true);
			}
		}
		self.details_focused = true;
		self.focus = focus;
	}

	pub fn is_info_focused(&self) -> bool {
		self.details_focused && self.focus == DetailsFocus::Info
	}

	pub fn is_message_focused(&self) -> bool {
		self.details_focused && self.focus == DetailsFocus::Message
	}

	pub fn set_commit(
		&mut self,
		id: Option<CommitId>,
		tags: Option<CommitTags>,
	) {
		self.tags.clear();

		self.data = id.and_then(|id| {
			sync::get_commit_details(&self.repo.borrow(), id).ok()
		});

		self.scroll.reset();
		self.info_scroll.reset();

		if let Some(tags) = tags {
			self.tags.extend(tags);
		}
	}

	fn wrap_commit_details(
		message: &CommitMessage,
		width: usize,
	) -> WrappedCommitMessage<'_> {
		let width = width.max(1);
		let wrapped_title = bwrap::wrap!(&message.subject, width)
			.lines()
			.map(String::from)
			.map(Cow::from)
			.collect();

		if let Some(ref body) = message.body {
			let wrapped_message: Vec<Cow<'_, str>> =
				bwrap::wrap!(body, width)
					.lines()
					.map(String::from)
					.map(Cow::from)
					.collect();

			(wrapped_title, wrapped_message)
		} else {
			(wrapped_title, vec![])
		}
	}

	fn get_wrapped_lines(
		data: Option<&CommitDetails>,
		width: usize,
	) -> WrappedCommitMessage<'_> {
		if let Some(data) = data {
			if let Some(message) = &data.message {
				return Self::wrap_commit_details(message, width);
			}
		}

		(vec![], vec![])
	}

	fn get_number_of_lines(
		details: Option<&CommitDetails>,
		width: usize,
	) -> usize {
		let (wrapped_title, wrapped_message) =
			Self::get_wrapped_lines(details, width);

		wrapped_title.len() + wrapped_message.len()
	}

	fn get_theme_for_line(&self, bold: bool) -> Style {
		if bold {
			self.theme.text(true, false).add_modifier(Modifier::BOLD)
		} else {
			self.theme.text(true, false)
		}
	}

	fn get_wrapped_text_message(
		&self,
		width: usize,
		height: usize,
	) -> Vec<Line<'_>> {
		let (wrapped_title, wrapped_message) =
			Self::get_wrapped_lines(self.data.as_ref(), width);

		[&wrapped_title[..], &wrapped_message[..]]
			.concat()
			.iter()
			.enumerate()
			.skip(self.scroll.get_top())
			.take(height)
			.map(|(i, line)| {
				Line::from(vec![Span::styled(
					line.clone(),
					self.get_theme_for_line(i < wrapped_title.len()),
				)])
			})
			.collect()
	}

	#[allow(clippy::too_many_lines)]
	fn get_text_info(&self) -> Vec<Line<'_>> {
		self.data.as_ref().map_or_else(Vec::new, |data| {
			let mut res = vec![
				Line::from(vec![
					style_detail(&self.theme, &Detail::Author),
					Span::styled(
						Cow::from(format!(
							"{} <{}>",
							data.author.name, data.author.email
						)),
						self.theme.text(true, false),
					),
				]),
				Line::from(vec![
					style_detail(&self.theme, &Detail::Date),
					Span::styled(
						Cow::from(time_to_string(
							data.author.time,
							false,
						)),
						self.theme.text(true, false),
					),
				]),
			];

			if let Some(ref committer) = data.committer {
				res.extend(vec![
					Line::from(vec![
						style_detail(&self.theme, &Detail::Committer),
						Span::styled(
							Cow::from(format!(
								"{} <{}>",
								committer.name, committer.email
							)),
							self.theme.text(true, false),
						),
					]),
					Line::from(vec![
						style_detail(&self.theme, &Detail::Date),
						Span::styled(
							Cow::from(time_to_string(
								committer.time,
								false,
							)),
							self.theme.text(true, false),
						),
					]),
				]);
			}

			res.push(Line::from(vec![
				Span::styled(
					Cow::from(strings::commit::details_sha()),
					self.theme.text(false, false),
				),
				Span::styled(
					Cow::from(data.hash.clone()),
					self.theme.text(true, false),
				),
			]));

			if !self.tags.is_empty() {
				let mut spans =
					vec![style_detail(&self.theme, &Detail::Sha)];
				for (i, tag) in self.tags.iter().enumerate() {
					if i > 0 {
						spans.push(Span::styled(
							Cow::from(", "),
							self.theme.text(true, false),
						));
					}
					spans.push(Span::styled(
						Cow::from(&tag.name),
						self.theme.text(true, false),
					));
				}
				res.push(Line::from(spans));
			}

			res
		})
	}

	fn move_scroll_top(&self, move_type: ScrollType) -> bool {
		if self.data.is_some() {
			self.scroll.move_top(move_type)
		} else {
			false
		}
	}

	fn move_info_scroll_top(&self, move_type: ScrollType) -> bool {
		if self.data.is_some() {
			self.info_scroll.move_top(move_type)
		} else {
			false
		}
	}
}

impl DrawableComponent for DetailsComponent {
	fn draw(&self, f: &mut Frame, rect: Rect) -> Result<()> {
		const CANSCROLL_STRING: &str = "[\u{2026}]";
		const EMPTY_STRING: &str = "";

		let info_focused = self.is_info_focused();
		let info_len = if info_focused { 12 } else { 8 };

		let chunks = Layout::default()
			.direction(Direction::Vertical)
			.constraints(
				[Constraint::Length(info_len), Constraint::Min(10)]
					.as_ref(),
			)
			.split(rect);

		// We have to take the border into account which is one
		// character on each side.
		let border_width: u16 = 2;

		let info_height =
			chunks[0].height.saturating_sub(border_width);

		let info_lines = self.get_text_info();
		let info_line_count = info_lines.len();

		self.info_scroll.update_no_selection(
			info_line_count,
			usize::from(info_height),
		);

		let info_visible: Vec<Line<'_>> = info_lines
			.iter()
			.skip(self.info_scroll.get_top())
			.take(usize::from(info_height))
			.cloned()
			.collect();

		let info_can_scroll =
			usize::from(info_height) < info_line_count;

		f.render_widget(
			dialog_paragraph(
				&format!(
					"{} {}",
					strings::commit::details_info_title(
						&self.key_config,
					),
					if !info_focused && info_can_scroll {
						CANSCROLL_STRING
					} else {
						EMPTY_STRING
					}
				),
				Text::from(info_visible),
				&self.theme,
				info_focused,
			),
			chunks[0],
		);

		if info_focused {
			self.info_scroll.draw(f, chunks[0], &self.theme);
		}

		let width = chunks[1].width.saturating_sub(border_width);
		let height = chunks[1].height.saturating_sub(border_width);

		self.current_width.set(width);

		let number_of_lines = Self::get_number_of_lines(
			self.data.as_ref(),
			usize::from(width),
		);

		self.scroll.update_no_selection(
			number_of_lines,
			usize::from(height),
		);

		if self.scroll_to_bottom_next_draw.get() {
			self.scroll.move_top(ScrollType::End);
			self.scroll_to_bottom_next_draw.set(false);
		}

		let message_focused = self.is_message_focused();
		let can_scroll = usize::from(height) < number_of_lines;

		f.render_widget(
			dialog_paragraph(
				&format!(
					"{} {}",
					strings::commit::details_message_title(
						&self.key_config,
					),
					if !message_focused && can_scroll {
						CANSCROLL_STRING
					} else {
						EMPTY_STRING
					}
				),
				Text::from(self.get_wrapped_text_message(
					width as usize,
					height as usize,
				)),
				&self.theme,
				message_focused,
			),
			chunks[1],
		);

		if message_focused {
			self.scroll.draw(f, chunks[1], &self.theme);
		}

		Ok(())
	}
}

impl Component for DetailsComponent {
	fn commands(
		&self,
		out: &mut Vec<CommandInfo>,
		force_all: bool,
	) -> CommandBlocking {
		let width = usize::from(self.current_width.get());
		let number_of_lines =
			Self::get_number_of_lines(self.data.as_ref(), width);

		let active = self.details_focused || force_all;

		match self.focus {
			DetailsFocus::Message => {
				out.push(
					CommandInfo::new(
						strings::commands::navigate_commit_message(
							&self.key_config,
						),
						number_of_lines > 0,
						active,
					)
					.order(order::NAV),
				);

				let has_message = self
					.data
					.as_ref()
					.and_then(|d| d.message.as_ref())
					.is_some();
				out.push(
					CommandInfo::new(
						strings::commands::copy_message(
							&self.key_config,
						),
						has_message,
						active,
					)
					.order(order::NAV),
				);
			}
			DetailsFocus::Info => {
				out.push(
					CommandInfo::new(
						strings::commands::navigate_commit_message(
							&self.key_config,
						),
						self.data.is_some(),
						active,
					)
					.order(order::NAV),
				);
				out.push(
					CommandInfo::new(
						strings::commands::copy_info(
							&self.key_config,
						),
						self.data.is_some(),
						active,
					)
					.order(order::NAV),
				);
			}
		}

		CommandBlocking::PassingOn
	}

	fn event(&mut self, event: &Event) -> Result<EventState> {
		if !self.details_focused {
			return Ok(EventState::NotConsumed);
		}

		if let Event::Key(e) = event {
			let keys = &self.key_config.keys;
			let scroll_key = key_match(e, keys.move_up)
				|| key_match(e, keys.popup_up)
				|| key_match(e, keys.move_down)
				|| key_match(e, keys.page_up)
				|| key_match(e, keys.page_down)
				|| key_match(e, keys.home)
				|| key_match(e, keys.end)
				|| key_match(e, keys.shift_up)
				|| key_match(e, keys.shift_down);

			match self.focus {
				DetailsFocus::Message => {
					if key_match(e, keys.copy) {
						self.copy_message();
						return Ok(EventState::Consumed);
					}
					if scroll_key {
						let moved = if key_match(e, keys.move_up)
							|| key_match(e, keys.popup_up)
						{
							self.move_scroll_top(ScrollType::Up)
						} else if key_match(e, keys.move_down) {
							self.move_scroll_top(ScrollType::Down)
						} else if key_match(e, keys.page_up) {
							self.move_scroll_top(ScrollType::PageUp)
						} else if key_match(e, keys.page_down) {
							self.move_scroll_top(ScrollType::PageDown)
						} else if key_match(e, keys.home)
							|| key_match(e, keys.shift_up)
						{
							self.move_scroll_top(ScrollType::Home)
						} else {
							self.move_scroll_top(ScrollType::End)
						};
						return Ok(moved.into());
					}
				}
				DetailsFocus::Info => {
					if key_match(e, keys.copy) {
						self.copy_info();
						return Ok(EventState::Consumed);
					}
					if scroll_key {
						let moved = if key_match(e, keys.move_up)
							|| key_match(e, keys.popup_up)
						{
							self.move_info_scroll_top(ScrollType::Up)
						} else if key_match(e, keys.move_down) {
							self.move_info_scroll_top(
								ScrollType::Down,
							)
						} else if key_match(e, keys.page_up) {
							self.move_info_scroll_top(
								ScrollType::PageUp,
							)
						} else if key_match(e, keys.page_down) {
							self.move_info_scroll_top(
								ScrollType::PageDown,
							)
						} else if key_match(e, keys.home)
							|| key_match(e, keys.shift_up)
						{
							self.move_info_scroll_top(
								ScrollType::Home,
							)
						} else {
							self.move_info_scroll_top(ScrollType::End)
						};
						return Ok(moved.into());
					}
				}
			}
		}

		Ok(EventState::NotConsumed)
	}

	fn focused(&self) -> bool {
		self.details_focused
	}

	fn focus(&mut self, focus: bool) {
		self.details_focused = focus;
		if focus {
			self.set_focus(DetailsFocus::Message);
		} else {
			self.scroll.reset();
			self.info_scroll.reset();
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn get_wrapped_lines(
		message: &CommitMessage,
		width: usize,
	) -> Vec<Cow<'_, str>> {
		let (wrapped_title, wrapped_message) =
			DetailsComponent::wrap_commit_details(message, width);

		[&wrapped_title[..], &wrapped_message[..]].concat()
	}

	#[test]
	fn test_textwrap() {
		let message = CommitMessage::from("Commit message");

		assert_eq!(
			get_wrapped_lines(&message, 7),
			vec!["Commit", "message"]
		);
		assert_eq!(
			get_wrapped_lines(&message, 14),
			vec!["Commit message"]
		);
		assert_eq!(
			get_wrapped_lines(&message, 0),
			vec!["Commit", "message"]
		);

		let message_with_newline =
			CommitMessage::from("Commit message\n");

		assert_eq!(
			get_wrapped_lines(&message_with_newline, 7),
			vec!["Commit", "message"]
		);
		assert_eq!(
			get_wrapped_lines(&message_with_newline, 14),
			vec!["Commit message"]
		);
		assert_eq!(
			get_wrapped_lines(&message, 0),
			vec!["Commit", "message"]
		);

		let message_with_body = CommitMessage::from(
			"Commit message\nFirst line\nSecond line",
		);

		assert_eq!(
			get_wrapped_lines(&message_with_body, 7),
			vec![
				"Commit", "message", "First", "line", "Second",
				"line"
			]
		);
		assert_eq!(
			get_wrapped_lines(&message_with_body, 14),
			vec!["Commit message", "First line", "Second line"]
		);
		assert_eq!(
			get_wrapped_lines(&message_with_body, 7),
			vec![
				"Commit", "message", "First", "line", "Second",
				"line"
			]
		);
	}
}

#[cfg(test)]
mod test_line_count {
	use super::*;

	#[test]
	fn test_smoke() {
		let commit = CommitDetails {
			message: Some(CommitMessage {
				subject: String::from("subject line"),
				body: Some(String::from("body lone")),
			}),
			..CommitDetails::default()
		};
		let lines = DetailsComponent::get_number_of_lines(
			Some(commit.clone()).as_ref(),
			50,
		);
		assert_eq!(lines, 2);

		let lines = DetailsComponent::get_number_of_lines(
			Some(commit).as_ref(),
			8,
		);
		assert_eq!(lines, 4);
	}
}
