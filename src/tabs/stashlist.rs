use crate::{
	app::Environment,
	components::{
		visibility_blocking, CommandBlocking, CommandInfo,
		CommitDetailsComponent, CommitList, Component,
		DrawableComponent, EventState,
	},
	keys::{key_match, SharedKeyConfig},
	options::SharedOptions,
	popups::InspectCommitOpen,
	queue::{Action, InternalEvent, Queue, StackablePopupOpen},
	strings,
};
use anyhow::Result;
use asyncgit::{
	sync::{self, CommitId, RepoPath, RepoPathRef},
	AsyncGitNotification, CommitFilesParams,
};
use crossterm::event::Event;
use ratatui::layout::{Constraint, Direction, Layout};

pub struct StashList {
	repo: RepoPathRef,
	list: CommitList,
	commit_details: CommitDetailsComponent,
	visible: bool,
	queue: Queue,
	key_config: SharedKeyConfig,
	options: SharedOptions,
}

impl StashList {
	///
	pub fn new(env: &Environment) -> Self {
		Self {
			visible: false,
			list: CommitList::new(
				env,
				&strings::stashlist_title(&env.key_config),
			),
			commit_details: CommitDetailsComponent::new(env),
			queue: env.queue.clone(),
			key_config: env.key_config.clone(),
			repo: env.repo.clone(),
			options: env.options.clone(),
		}
	}

	///
	pub fn update(&mut self) -> Result<()> {
		if self.is_visible() {
			let stashes = sync::get_stashes(&self.repo.borrow())?;
			self.list.set_commits(stashes.into_iter().collect());

			if self.commit_details.is_visible() {
				let commit = self.selected_commit();
				self.commit_details.set_commits(
					commit.map(CommitFilesParams::from),
					None,
				)?;
			}
		}

		Ok(())
	}

	///
	pub fn update_git(
		&mut self,
		ev: AsyncGitNotification,
	) -> Result<()> {
		if self.visible
			&& matches!(ev, AsyncGitNotification::CommitFiles)
		{
			self.update()?;
		}

		Ok(())
	}

	fn selected_commit(&self) -> Option<CommitId> {
		self.list.selected_entry().map(|e| e.id)
	}

	fn apply_stash(&self) {
		if let Some(e) = self.list.selected_entry() {
			self.queue.push(InternalEvent::ConfirmAction(
				Action::StashApply(e.id),
			));
		}
	}

	fn drop_stash(&self) {
		if self.list.marked_count() > 0 {
			self.queue.push(InternalEvent::ConfirmAction(
				Action::StashDrop(self.list.marked_commits()),
			));
		} else if let Some(e) = self.list.selected_entry() {
			self.queue.push(InternalEvent::ConfirmAction(
				Action::StashDrop(vec![e.id]),
			));
		}
	}

	fn pop_stash(&self) {
		if let Some(e) = self.list.selected_entry() {
			self.queue.push(InternalEvent::ConfirmAction(
				Action::StashPop(e.id),
			));
		}
	}

	fn inspect(&self) {
		if let Some(e) = self.list.selected_entry() {
			self.queue.push(InternalEvent::OpenPopup(
				StackablePopupOpen::InspectCommit(
					InspectCommitOpen::new(e.id),
				),
			));
		}
	}

	/// Called when a pending stash action has been confirmed
	pub fn action_confirmed(
		&mut self,
		repo: &RepoPath,
		action: &Action,
	) -> Result<()> {
		match action {
			Action::StashDrop(ids) => self.drop(repo, ids)?,
			Action::StashPop(id) => self.pop(repo, *id)?,
			Action::StashApply(id) => self.apply(repo, *id)?,
			_ => (),
		}

		Ok(())
	}

	fn drop(
		&mut self,
		repo: &RepoPath,
		ids: &[CommitId],
	) -> Result<()> {
		for id in ids {
			sync::stash_drop(repo, *id)?;
		}

		self.list.clear_marked();
		self.update()?;

		Ok(())
	}

	fn pop(&mut self, repo: &RepoPath, id: CommitId) -> Result<()> {
		sync::stash_pop(repo, id)?;

		self.list.clear_marked();
		self.update()?;

		self.queue.push(InternalEvent::TabSwitchStatus);

		Ok(())
	}

	fn apply(&mut self, repo: &RepoPath, id: CommitId) -> Result<()> {
		sync::stash_apply(repo, id, false)?;

		self.update()?;

		self.queue.push(InternalEvent::TabSwitchStatus);

		Ok(())
	}
}

impl DrawableComponent for StashList {
	fn draw(
		&self,
		f: &mut ratatui::Frame,
		rect: ratatui::layout::Rect,
	) -> Result<()> {
		if self.commit_details.is_visible() {
			let left_ratio = self.options.borrow().log_left_ratio();
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
				.split(rect);

			self.list.draw(f, chunks[0])?;
			self.commit_details.draw(f, chunks[1])?;
			self.commit_details.files().draw_popup(f)?;
		} else {
			self.list.draw(f, rect)?;
		}

		Ok(())
	}
}

impl Component for StashList {
	fn commands(
		&self,
		out: &mut Vec<CommandInfo>,
		force_all: bool,
	) -> CommandBlocking {
		if self.visible || force_all {
			self.list.commands(out, force_all);

			let selection_valid =
				self.list.selected_entry().is_some();
			out.push(CommandInfo::new(
				strings::commands::stashlist_pop(&self.key_config),
				selection_valid,
				true,
			));
			out.push(CommandInfo::new(
				strings::commands::stashlist_apply(&self.key_config),
				selection_valid,
				true,
			));
			out.push(CommandInfo::new(
				strings::commands::stashlist_drop(
					&self.key_config,
					self.list.marked_count(),
				),
				selection_valid,
				true,
			));
			out.push(CommandInfo::new(
				strings::commands::stashlist_details_toggle(
					&self.key_config,
				),
				selection_valid,
				true,
			));
			out.push(CommandInfo::new(
				strings::commands::stashlist_inspect(
					&self.key_config,
				),
				selection_valid,
				self.commit_details.is_visible(),
			));
		}

		visibility_blocking(self)
	}

	fn event(
		&mut self,
		ev: &crossterm::event::Event,
	) -> Result<EventState> {
		if self.is_visible() {
			if self.list.event(ev)?.is_consumed() {
				self.update()?;
				return Ok(EventState::Consumed);
			}

			if let Event::Key(k) = ev {
				if key_match(k, self.key_config.keys.enter) {
					self.commit_details.toggle_visible()?;
					self.update()?;
					return Ok(EventState::Consumed);
				} else if key_match(k, self.key_config.keys.stash_pop)
				{
					self.pop_stash();
				} else if key_match(
					k,
					self.key_config.keys.stash_apply,
				) {
					self.apply_stash();
				} else if key_match(
					k,
					self.key_config.keys.stash_drop,
				) {
					self.drop_stash();
				} else if key_match(
					k,
					self.key_config.keys.move_right,
				) && self.commit_details.is_visible()
				{
					self.inspect();
					return Ok(EventState::Consumed);
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
		self.update()?;
		Ok(())
	}
}
