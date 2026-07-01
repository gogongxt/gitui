mod compare_details;
mod details;
mod style;

use super::{
	command_pump, event_pump, CommandBlocking, CommandInfo,
	Component, DrawableComponent, EventState, StatusTreeComponent,
};
use crate::{
	accessors,
	app::Environment,
	keys::{key_match, SharedKeyConfig},
	strings,
};
use anyhow::Result;
use asyncgit::{
	sync::{commit_files::OldNew, CommitTags},
	AsyncCommitFiles, CommitFilesParams,
};
use compare_details::CompareDetailsComponent;
use crossterm::event::Event;
use details::{DetailsComponent, DetailsFocus};
use ratatui::{
	layout::{Constraint, Direction, Layout, Rect},
	Frame,
};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Focus {
	None,
	Info,
	Message,
	Files,
}

pub struct CommitDetailsComponent {
	commit: Option<CommitFilesParams>,
	single_details: DetailsComponent,
	compare_details: CompareDetailsComponent,
	file_tree: StatusTreeComponent,
	git_commit_files: AsyncCommitFiles,
	visible: bool,
	focus: Focus,
	key_config: SharedKeyConfig,
}

impl CommitDetailsComponent {
	accessors!(self, [single_details, compare_details, file_tree]);

	///
	pub fn new(env: &Environment) -> Self {
		Self {
			single_details: DetailsComponent::new(env, false),
			compare_details: CompareDetailsComponent::new(env, false),
			git_commit_files: AsyncCommitFiles::new(
				env.repo.borrow().clone(),
				&env.sender_git,
			),
			file_tree: StatusTreeComponent::new(env, "", false),
			visible: false,
			focus: Focus::None,
			commit: None,
			key_config: env.key_config.clone(),
		}
	}

	fn get_files_title(&self) -> String {
		let files_count = self.file_tree.file_count();

		format!(
			"{} {}",
			strings::commit::details_files_title(&self.key_config),
			files_count
		)
	}

	///
	pub fn set_commits(
		&mut self,
		params: Option<CommitFilesParams>,
		tags: Option<&CommitTags>,
	) -> Result<()> {
		if params.is_none() {
			self.single_details.set_commit(None, None);
			self.compare_details.set_commits(None);
		}

		self.commit = params;

		if let Some(id) = params {
			self.file_tree.set_commit(Some(id.id));

			if let Some(other) = id.other {
				self.compare_details.set_commits(Some(OldNew {
					new: id.id,
					old: other,
				}));
			} else {
				self.single_details
					.set_commit(Some(id.id), tags.cloned());
			}

			if let Some((fetched_id, res)) =
				self.git_commit_files.current()?
			{
				if fetched_id == id {
					self.file_tree.update(res.as_slice())?;
					self.file_tree.set_title(self.get_files_title());

					return Ok(());
				}
			}

			self.file_tree.clear()?;
			self.git_commit_files.fetch(id)?;
		}

		self.file_tree.set_title(self.get_files_title());

		Ok(())
	}

	///
	pub fn any_work_pending(&self) -> bool {
		self.git_commit_files.is_pending()
	}

	///
	pub const fn files(&self) -> &StatusTreeComponent {
		&self.file_tree
	}

	fn set_focus(&mut self, focus: Focus) {
		// Unfocus everything first.
		self.single_details.focus(false);
		self.compare_details.focus(false);
		self.file_tree.focus(false);

		match focus {
			Focus::None => {}
			Focus::Info => {
				if self.is_compare() {
					// Compare mode has no Info pane; fall back to
					// Message.
					self.compare_details.focus(true);
					self.focus = Focus::Message;
					return;
				}
				self.single_details.set_focus(DetailsFocus::Info);
			}
			Focus::Message => {
				if self.is_compare() {
					self.compare_details.focus(true);
				} else {
					self.single_details
						.set_focus(DetailsFocus::Message);
				}
			}
			Focus::Files => {
				self.file_tree.focus(true);
				self.file_tree.show_selection(true);
			}
		}

		self.focus = focus;
	}

	/// Unfocus both the message pane and the file tree. Used when the
	/// inline details panel is opened from the Revlog tab so that focus
	/// stays on the commit list and `y` copies the commit hash. To copy
	/// the commit message instead, open the fullscreen `InspectCommit`
	/// popup (right-arrow), which focuses the message pane.
	pub fn focus_details(&mut self) {
		self.set_focus(Focus::None);
	}

	fn is_compare(&self) -> bool {
		self.commit.is_some_and(|p| p.other.is_some())
	}
}

impl DrawableComponent for CommitDetailsComponent {
	fn draw(&self, f: &mut Frame, rect: Rect) -> Result<()> {
		if !self.visible {
			return Ok(());
		}

		let constraints = if self.is_compare() {
			[Constraint::Length(10), Constraint::Min(0)]
		} else {
			let percentages = match self.focus {
				Focus::Info | Focus::Message => (60, 40),
				Focus::Files | Focus::None => (40, 60),
			};

			[
				Constraint::Percentage(percentages.0),
				Constraint::Percentage(percentages.1),
			]
		};

		let chunks = Layout::default()
			.direction(Direction::Vertical)
			.constraints(constraints.as_ref())
			.split(rect);

		if self.is_compare() {
			self.compare_details.draw(f, chunks[0])?;
		} else {
			self.single_details.draw(f, chunks[0])?;
		}
		self.file_tree.draw(f, chunks[1])?;

		Ok(())
	}
}

impl Component for CommitDetailsComponent {
	fn commands(
		&self,
		out: &mut Vec<CommandInfo>,
		force_all: bool,
	) -> CommandBlocking {
		if self.visible || force_all {
			command_pump(
				out,
				force_all,
				self.components().as_slice(),
			);
		}

		CommandBlocking::PassingOn
	}

	fn event(&mut self, ev: &Event) -> Result<EventState> {
		if event_pump(ev, self.components_mut().as_mut_slice())?
			.is_consumed()
		{
			if !self.file_tree.is_visible() {
				self.hide();
			}

			return Ok(EventState::Consumed);
		}

		if self.focused() && !self.is_compare() {
			if let Event::Key(e) = ev {
				let down =
					key_match(e, self.key_config.keys.move_down)
						|| key_match(
							e,
							self.key_config.keys.popup_down,
						);
				let up = key_match(e, self.key_config.keys.move_up)
					|| key_match(e, self.key_config.keys.popup_up);

				if down {
					let next = match self.focus {
						Focus::Info => Some(Focus::Message),
						Focus::Message => Some(Focus::Files),
						Focus::Files | Focus::None => None,
					};
					if let Some(next) = next {
						self.set_focus(next);
						return Ok(EventState::Consumed);
					}
				} else if up {
					let prev = match self.focus {
						Focus::Files => Some(Focus::Message),
						Focus::Message => Some(Focus::Info),
						Focus::Info | Focus::None => None,
					};
					if let Some(prev) = prev {
						self.set_focus(prev);
						return Ok(EventState::Consumed);
					}
				}
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
		self.file_tree.show()?;
		Ok(())
	}

	fn focused(&self) -> bool {
		self.focus != Focus::None
	}

	fn focus(&mut self, focus: bool) {
		if focus {
			self.set_focus(Focus::Files);
		} else {
			self.set_focus(Focus::None);
		}
	}
}
