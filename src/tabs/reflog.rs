use crate::{
	app::Environment,
	components::{
		visibility_blocking, CommandBlocking, CommandInfo,
		CommitDetailsComponent, Component, DrawableComponent,
		EventState, ScrollType,
	},
	keys::{key_match, SharedKeyConfig},
	options::SharedOptions,
	popups::InspectCommitOpen,
	queue::{InternalEvent, Queue, StackablePopupOpen},
	strings, try_or_popup,
	ui::style::SharedTheme,
};
use anyhow::Result;
use asyncgit::{
	sync::{ReflogAction, ReflogEntry},
	AsyncGitNotification, AsyncReflog, CommitFilesParams,
	ReflogFetchStatus,
};
use crossterm::event::Event;
use ratatui::{
	layout::{Alignment, Constraint, Direction, Layout, Rect},
	style::{Color, Modifier, Style},
	text::Span,
	widgets::{Block, Borders, Cell, Row, Table},
	Frame,
};
use std::{
	cell::{Cell as StdCell, RefCell},
	cmp,
	rc::Rc,
	time::Instant,
};

/// Get style for reflog action type
fn action_style(action: &ReflogAction, selected: bool) -> Style {
	let base_style = if selected {
		Style::default().bg(Color::Blue)
	} else {
		Style::default()
	};

	let color = match action {
		ReflogAction::Commit => Color::Green,
		ReflogAction::Checkout { .. } => Color::Cyan,
		ReflogAction::Reset { .. } => Color::Yellow,
		ReflogAction::Rebase => Color::Magenta,
		ReflogAction::Merge | ReflogAction::CherryPick => {
			Color::LightMagenta
		}
		ReflogAction::Pull => Color::LightCyan,
		ReflogAction::Clone | ReflogAction::Amend => {
			Color::LightGreen
		}
		ReflogAction::Branch => Color::LightYellow,
		ReflogAction::Unknown => Color::DarkGray,
	};

	base_style.fg(color).add_modifier(Modifier::BOLD)
}

///
pub struct Reflog {
	#[allow(dead_code)]
	repo: asyncgit::sync::RepoPathRef,
	#[allow(clippy::struct_field_names)]
	git_reflog: AsyncReflog,
	commit_details: CommitDetailsComponent,
	entries: Rc<RefCell<Vec<ReflogEntry>>>,
	selected: usize,
	scroll_state: (Instant, f32),
	scroll_top: StdCell<usize>,
	current_size: StdCell<Option<(u16, u16)>>,
	queue: Queue,
	visible: bool,
	key_config: SharedKeyConfig,
	theme: SharedTheme,
	options: SharedOptions,
}

///
impl Reflog {
	///
	pub fn new(env: &Environment) -> Self {
		Self {
			repo: env.repo.clone(),
			git_reflog: AsyncReflog::new(
				env.repo.borrow().clone(),
				&env.sender_git,
			),
			commit_details: CommitDetailsComponent::new(env),
			entries: Rc::new(RefCell::new(Vec::new())),
			selected: 0,
			scroll_state: (Instant::now(), 0_f32),
			scroll_top: StdCell::new(0),
			current_size: StdCell::new(None),
			queue: env.queue.clone(),
			visible: false,
			key_config: env.key_config.clone(),
			theme: env.theme.clone(),
			options: env.options.clone(),
		}
	}

	///
	pub fn any_work_pending(&self) -> bool {
		self.git_reflog.is_pending()
			|| self.commit_details.any_work_pending()
	}

	///
	pub fn update(&mut self) -> Result<()> {
		if self.visible {
			// Only fetch if reflog changed (similar to Revlog pattern)
			// This prevents unnecessary refresh/flicker
			if self.git_reflog.fetch()? == ReflogFetchStatus::Started
			{
				// Clear old data when new fetch started
				self.entries.borrow_mut().clear();
			}

			// Always get current items
			let items = self.git_reflog.get_items()?;
			if !items.is_empty() {
				let was_empty = self.entries.borrow().is_empty();
				*self.entries.borrow_mut() = items;

				// Only reset selection on first load
				if was_empty {
					self.selected = 0;
				}

				// Clamp selection to valid range
				let len = self.entries.borrow().len();
				if self.selected >= len && len > 0 {
					self.selected = len - 1;
				}
			}

			// Update commit details if visible
			if self.commit_details.is_visible() {
				if let Some(entry) = self.selected_entry() {
					self.commit_details.set_commits(
						Some(CommitFilesParams::from(entry.new_oid)),
						None,
					)?;
				}
			}
		}

		Ok(())
	}

	///
	pub fn update_git(
		&mut self,
		ev: AsyncGitNotification,
	) -> Result<()> {
		if self.visible {
			match ev {
				AsyncGitNotification::Reflog
				| AsyncGitNotification::CommitFiles => self.update()?,
				_ => (),
			}
		}

		Ok(())
	}

	fn selection_max(&self) -> usize {
		self.entries.borrow().len().saturating_sub(1)
	}

	/// Update scroll speed for smooth acceleration
	fn update_scroll_speed(&mut self) {
		const REPEATED_SCROLL_THRESHOLD_MILLIS: u128 = 300;
		const SCROLL_SPEED_START: f32 = 0.1_f32;
		const SCROLL_SPEED_MAX: f32 = 10_f32;
		const SCROLL_SPEED_MULTIPLIER: f32 = 1.05_f32;

		let now = Instant::now();
		let since_last_scroll =
			now.duration_since(self.scroll_state.0);

		self.scroll_state.0 = now;

		let speed = if since_last_scroll.as_millis()
			< REPEATED_SCROLL_THRESHOLD_MILLIS
		{
			self.scroll_state.1 * SCROLL_SPEED_MULTIPLIER
		} else {
			SCROLL_SPEED_START
		};

		self.scroll_state.1 = speed.min(SCROLL_SPEED_MAX);
	}

	/// Move selection with scroll acceleration
	fn move_selection(&mut self, scroll: ScrollType) -> bool {
		self.update_scroll_speed();

		#[allow(clippy::cast_possible_truncation)]
		let speed_int = usize::try_from(self.scroll_state.1 as i64)
			.unwrap_or(1)
			.max(1);

		let page_offset = usize::from(
			self.current_size.get().unwrap_or_default().1,
		)
		.saturating_sub(1);

		let new_selection = match scroll {
			ScrollType::Up => self.selected.saturating_sub(speed_int),
			ScrollType::Down => {
				self.selected.saturating_add(speed_int)
			}
			ScrollType::PageUp => {
				self.selected.saturating_sub(page_offset)
			}
			ScrollType::PageDown => {
				self.selected.saturating_add(page_offset)
			}
			ScrollType::Home => 0,
			ScrollType::End => self.selection_max(),
		};

		let new_selection =
			cmp::min(new_selection, self.selection_max());
		let needs_update = new_selection != self.selected;

		self.selected = new_selection;

		needs_update
	}

	/// Get the currently selected entry
	fn selected_entry(&self) -> Option<ReflogEntry> {
		let entries = self.entries.borrow();
		entries.get(self.selected).cloned()
	}

	/// Update commit details if visible
	fn update_details(&mut self) -> Result<()> {
		if self.commit_details.is_visible() {
			if let Some(entry) = self.selected_entry() {
				self.commit_details.set_commits(
					Some(CommitFilesParams::from(entry.new_oid)),
					None,
				)?;
			}
		}
		Ok(())
	}

	fn inspect_commit(&self) {
		if let Some(entry) = self.selected_entry() {
			self.queue.push(InternalEvent::OpenPopup(
				StackablePopupOpen::InspectCommit(
					InspectCommitOpen::new(entry.new_oid),
				),
			));
		}
	}

	fn copy_commit_hash(&self) -> Result<()> {
		if let Some(entry) = self.selected_entry() {
			let hash = entry.new_oid.to_string();
			crate::clipboard::copy_string(&hash)?;
			self.queue.push(InternalEvent::ShowInfoMsg(
				strings::copy_success(&hash),
			));
		}
		Ok(())
	}

	/// Create a table row for a reflog entry
	fn create_row(
		&self,
		entry: &ReflogEntry,
		is_selected: bool,
	) -> Row<'_> {
		let normal = !self.git_reflog.is_pending() || is_selected;

		let style_hash = self.theme.commit_hash(is_selected);
		let style_action = action_style(&entry.action, is_selected);
		let style_time = self.theme.commit_time(is_selected);
		let style_author = self.theme.commit_author(is_selected);
		let style_msg = self.theme.text(normal, is_selected);

		let time_str =
			crate::components::time_to_string(entry.timestamp, true);
		let action_str = entry.action.short_name();
		let row_style = self.theme.text(true, is_selected);

		Row::new(vec![
			Cell::from(Span::styled(
				entry.new_oid.get_short_string(),
				style_hash,
			)),
			Cell::from(Span::styled(time_str, style_time)),
			Cell::from(Span::styled(
				entry.committer.clone(),
				style_author,
			)),
			Cell::from(Span::styled(action_str, style_action)),
			Cell::from(Span::styled(
				entry.message.clone(),
				style_msg,
			)),
			Cell::from(Span::styled(
				entry.commit_subject.clone(),
				style_msg,
			)),
		])
		.style(row_style)
	}

	fn draw_table(&self, f: &mut Frame, area: Rect) {
		let current_size = (
			area.width.saturating_sub(2),
			area.height.saturating_sub(2),
		);
		self.current_size.set(Some(current_size));

		let entries = self.entries.borrow();
		let height_in_lines = current_size.1 as usize;

		self.scroll_top.set(crate::ui::calc_scroll_top(
			self.scroll_top.get(),
			height_in_lines,
			self.selected,
		));

		let title = format!(
			"{} {}/{}",
			strings::reflog_title(&self.key_config),
			entries.len().saturating_sub(self.selected),
			entries.len(),
		);
		let block = Block::default()
			.title(title)
			.borders(Borders::ALL)
			.border_style(self.theme.block(true));

		if entries.is_empty() {
			let text = if self.git_reflog.is_pending() {
				"Loading..."
			} else {
				"No reflog entries"
			};
			let paragraph = ratatui::widgets::Paragraph::new(text)
				.block(block)
				.alignment(Alignment::Center);
			f.render_widget(paragraph, area);
		} else {
			let selected_idx = self.selected;
			let scroll_top = self.scroll_top.get();
			let rows: Vec<Row> = entries
				.iter()
				.skip(scroll_top)
				.take(height_in_lines)
				.enumerate()
				.map(|(idx, entry)| {
					let actual_idx = idx + scroll_top;
					self.create_row(entry, actual_idx == selected_idx)
				})
				.collect();

			let widths = [
				Constraint::Length(7),
				Constraint::Length(10),
				Constraint::Length(12),
				Constraint::Length(10),
				Constraint::Min(15),
				Constraint::Min(20),
			];

			let table = Table::new(rows, widths)
				.block(block)
				.column_spacing(1);

			f.render_widget(table, area);

			crate::ui::draw_scrollbar(
				f,
				area,
				&self.theme,
				entries.len(),
				self.selected,
				crate::ui::Orientation::Vertical,
			);
		}
	}
}

impl DrawableComponent for Reflog {
	fn draw(&self, f: &mut Frame, area: Rect) -> Result<()> {
		if self.visible {
			if self.commit_details.is_visible() {
				let left_ratio =
					self.options.borrow().log_left_ratio();
				let right_ratio = 100 - left_ratio;

				let chunks = Layout::default()
					.direction(Direction::Horizontal)
					.constraints(
						[
							Constraint::Percentage(left_ratio),
							Constraint::Percentage(right_ratio),
						]
						.as_ref(),
					)
					.split(area);

				self.draw_table(f, chunks[0]);
				self.commit_details.draw(f, chunks[1])?;
				self.commit_details.files().draw_popup(f)?;
			} else {
				self.draw_table(f, area);
			}
		}

		Ok(())
	}
}

impl Component for Reflog {
	fn commands(
		&self,
		out: &mut Vec<CommandInfo>,
		force_all: bool,
	) -> CommandBlocking {
		if self.visible || force_all {
			out.push(CommandInfo::new(
				strings::commands::navigate_commit_message(
					&self.key_config,
				),
				true,
				self.visible,
			));

			out.push(CommandInfo::new(
				strings::commands::log_details_toggle(
					&self.key_config,
				),
				true,
				self.visible,
			));

			out.push(CommandInfo::new(
				strings::commands::commit_details_open(
					&self.key_config,
				),
				true,
				(self.visible && self.commit_details.is_visible())
					|| force_all,
			));

			out.push(CommandInfo::new(
				strings::commands::copy_hash(&self.key_config),
				self.selected_entry().is_some(),
				self.visible || force_all,
			));
		}

		visibility_blocking(self)
	}

	fn event(&mut self, ev: &Event) -> Result<EventState> {
		if self.visible {
			// Let commit details handle events first if it's focused
			if self.commit_details.focused() {
				let event_used = self.commit_details.event(ev)?;
				if event_used.is_consumed() {
					return Ok(EventState::Consumed);
				}
			}

			if let Event::Key(k) = ev {
				// Allow tab switching keys to pass through to app
				if key_match(k, self.key_config.keys.tab_status)
					|| key_match(k, self.key_config.keys.tab_log)
					|| key_match(k, self.key_config.keys.tab_files)
					|| key_match(k, self.key_config.keys.tab_stashing)
					|| key_match(k, self.key_config.keys.tab_stashes)
					|| key_match(k, self.key_config.keys.tab_reflog)
				{
					return Ok(EventState::NotConsumed);
				}

				if key_match(k, self.key_config.keys.exit_popup) {
					if self.commit_details.is_visible() {
						self.commit_details.hide();
					}
					return Ok(EventState::Consumed);
				} else if key_match(k, self.key_config.keys.enter) {
					// Toggle commit details panel
					if self.commit_details.is_visible() {
						self.commit_details.hide();
					} else if self.selected_entry().is_some() {
						self.commit_details.show()?;
						self.update_details()?;
					}
					return Ok(EventState::Consumed);
				} else if key_match(k, self.key_config.keys.move_up) {
					self.move_selection(ScrollType::Up);
					self.update_details()?;
					return Ok(EventState::Consumed);
				} else if key_match(k, self.key_config.keys.move_down)
				{
					self.move_selection(ScrollType::Down);
					self.update_details()?;
					return Ok(EventState::Consumed);
				} else if key_match(k, self.key_config.keys.home) {
					self.move_selection(ScrollType::Home);
					self.update_details()?;
					return Ok(EventState::Consumed);
				} else if key_match(k, self.key_config.keys.end) {
					self.move_selection(ScrollType::End);
					self.update_details()?;
					return Ok(EventState::Consumed);
				} else if key_match(k, self.key_config.keys.page_up) {
					self.move_selection(ScrollType::PageUp);
					self.update_details()?;
					return Ok(EventState::Consumed);
				} else if key_match(k, self.key_config.keys.page_down)
				{
					self.move_selection(ScrollType::PageDown);
					self.update_details()?;
					return Ok(EventState::Consumed);
				} else if key_match(
					k,
					self.key_config.keys.move_right,
				) && self.commit_details.is_visible()
				{
					self.inspect_commit();
					return Ok(EventState::Consumed);
				} else if key_match(k, self.key_config.keys.copy) {
					try_or_popup!(
						self,
						strings::POPUP_FAIL_COPY,
						self.copy_commit_hash()
					);
					return Ok(EventState::Consumed);
				}
			}

			return Ok(EventState::Consumed);
		}

		Ok(EventState::NotConsumed)
	}

	fn is_visible(&self) -> bool {
		self.visible
	}

	fn hide(&mut self) {
		self.visible = false;
		self.commit_details.hide();
	}

	fn show(&mut self) -> Result<()> {
		self.visible = true;
		self.update()?;
		Ok(())
	}
}
