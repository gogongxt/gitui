use super::{
	utils::scroll_vertical::VerticalScroll, CommandBlocking,
	CommandInfo, Component, DrawableComponent, EventState,
	FuzzyFinderTarget, SyntaxTextComponent,
};
use crate::{
	app::Environment,
	keys::{key_match, SharedKeyConfig},
	options::SharedOptions,
	popups::{BlameFileOpen, CopyPathPopup, FileRevOpen},
	queue::{InternalEvent, Queue, StackablePopupOpen},
	strings::{self, order, symbol},
	ui::{self, common_nav, style::SharedTheme},
	AsyncNotification,
};
use anyhow::Result;
use asyncgit::{
	asyncjob::AsyncSingleJob,
	sync::{
		get_commit_info, tree_file_content, utils::repo_work_dir,
		CommitId, CommitInfo, RepoPathRef, TreeFile,
	},
	AsyncGitNotification, AsyncTreeFilesJob,
};
use crossterm::event::Event;
use filetreelist::{FileTree, FileTreeItem};
use ratatui::{
	layout::{Constraint, Direction, Layout, Rect},
	text::{Line, Span},
	widgets::{Block, Borders},
	Frame,
};
use std::{borrow::Cow, fmt::Write};
use std::{
	collections::BTreeSet,
	path::{Path, PathBuf},
};
use unicode_truncate::UnicodeTruncateStr;
use unicode_width::UnicodeWidthStr;

enum Focus {
	Tree,
	File,
}

pub struct RevisionFilesComponent {
	repo: RepoPathRef,
	queue: Queue,
	theme: SharedTheme,
	//TODO: store TreeFiles in `tree`
	files: Option<Vec<TreeFile>>,
	async_treefiles: AsyncSingleJob<AsyncTreeFilesJob>,
	current_file: SyntaxTextComponent,
	tree: FileTree,
	scroll: VerticalScroll,
	visible: bool,
	revision: Option<CommitInfo>,
	focus: Focus,
	key_config: SharedKeyConfig,
	select_file: Option<PathBuf>,
	options: SharedOptions,
	copy_path_popup: CopyPathPopup,
}

impl RevisionFilesComponent {
	///
	pub fn new(
		env: &Environment,
		select_file: Option<PathBuf>,
	) -> Self {
		Self {
			queue: env.queue.clone(),
			tree: FileTree::default(),
			scroll: VerticalScroll::new(),
			current_file: SyntaxTextComponent::new(env),
			theme: env.theme.clone(),
			files: None,
			async_treefiles: AsyncSingleJob::new(
				env.sender_git.clone(),
			),
			revision: None,
			focus: Focus::Tree,
			key_config: env.key_config.clone(),
			repo: env.repo.clone(),
			select_file,
			visible: false,
			options: env.options.clone(),
			copy_path_popup: CopyPathPopup::new(
				env.queue.clone(),
				env.theme.clone(),
				env.key_config.clone(),
			),
		}
	}

	///
	pub fn set_commit(&mut self, commit: CommitId) -> Result<()> {
		self.show()?;

		let same_id =
			self.revision.as_ref().is_some_and(|c| c.id == commit);

		if !same_id {
			self.files = None;

			self.request_files(commit);

			self.revision =
				Some(get_commit_info(&self.repo.borrow(), &commit)?);
		}

		Ok(())
	}

	///
	pub const fn revision(&self) -> Option<&CommitInfo> {
		self.revision.as_ref()
	}

	///
	pub fn update(&mut self, ev: AsyncNotification) -> Result<()> {
		self.current_file.update(ev);

		if matches!(
			ev,
			AsyncNotification::Git(AsyncGitNotification::TreeFiles)
		) {
			self.refresh_files()?;
		}

		Ok(())
	}

	fn refresh_files(&mut self) -> Result<(), anyhow::Error> {
		if let Some(last) = self.async_treefiles.take_last() {
			if let Some(result) = last.result() {
				if self
					.revision
					.as_ref()
					.is_some_and(|commit| commit.id == result.commit)
				{
					if let Ok(last) = result.result {
						let filenames: Vec<&Path> = last
							.iter()
							.map(|f| f.path.as_path())
							.collect();
						self.tree = FileTree::new(
							&filenames,
							&BTreeSet::new(),
						)?;
						self.tree.collapse_but_root();

						self.files = Some(last);

						let select_file = self.select_file.clone();
						self.select_file = None;
						if let Some(file) = select_file {
							self.find_file(file.as_path());
						}
					}
				} else if let Some(rev) = &self.revision {
					self.request_files(rev.id);
				}
			}
		}

		Ok(())
	}

	///
	pub fn any_work_pending(&self) -> bool {
		self.current_file.any_work_pending()
			|| self.async_treefiles.is_pending()
	}

	fn tree_item_to_span<'a>(
		item: &'a FileTreeItem,
		theme: &SharedTheme,
		width: usize,
		selected: bool,
	) -> Span<'a> {
		let path = item.info().path_str();
		let indent = item.info().indent();

		let indent_str = if indent == 0 {
			String::new()
		} else {
			format!("{:w$}", " ", w = (indent as usize) * 2)
		};

		let is_path = item.kind().is_path();
		let path_arrow = if is_path {
			if item.kind().is_path_collapsed() {
				symbol::FOLDER_ICON_COLLAPSED
			} else {
				symbol::FOLDER_ICON_EXPANDED
			}
		} else {
			symbol::EMPTY_STR
		};

		let available_width =
			width.saturating_sub(indent_str.len() + path_arrow.len());

		let path = format!(
			"{indent_str}{path_arrow}{path:available_width$}"
		);

		Span::styled(path, theme.file_tree_item(is_path, selected))
	}

	fn blame(&self) -> bool {
		self.selected_file_path().is_some_and(|path| {
			self.queue.push(InternalEvent::OpenPopup(
				StackablePopupOpen::BlameFile(BlameFileOpen {
					file_path: path,
					commit_id: self.revision.as_ref().map(|c| c.id),
					selection: None,
				}),
			));

			true
		})
	}

	fn file_history(&self) -> bool {
		self.selected_file_path().is_some_and(|path| {
			self.queue.push(InternalEvent::OpenPopup(
				StackablePopupOpen::FileRevlog(FileRevOpen::new(
					path,
				)),
			));

			true
		})
	}

	fn open_finder(&self) {
		if let Some(files) = self.files.clone() {
			self.queue.push(InternalEvent::OpenFuzzyFinder(
				files
					.iter()
					.map(|a| {
						a.path
							.to_str()
							.unwrap_or_default()
							.to_string()
					})
					.collect(),
				FuzzyFinderTarget::Files,
			));
		}
	}

	pub fn find_file(&mut self, file: &Path) {
		self.tree.collapse_but_root();
		if self.tree.select_file(file) {
			self.selection_changed();
		}
	}

	fn selected_file_path_with_prefix(&self) -> Option<String> {
		self.tree
			.selected_file()
			.map(|file| file.full_path_str().to_string())
	}

	fn selected_file_path(&self) -> Option<String> {
		self.tree.selected_file().map(|file| {
			file.full_path_str()
				.strip_prefix("./")
				.unwrap_or_default()
				.to_string()
		})
	}

	fn selected_item_path(&self) -> Option<String> {
		self.tree.selected_item().map(|item| {
			item.full_path_str()
				.strip_prefix("./")
				.unwrap_or_default()
				.to_string()
		})
	}

	fn open_copy_path_popup(&mut self) {
		if let Some(relative_path) = self.selected_item_path() {
			let absolute_path = repo_work_dir(&self.repo.borrow())
				.map_or_else(
					|_| relative_path.clone(),
					|work_dir| {
						Path::new(&work_dir)
							.join(&relative_path)
							.to_string_lossy()
							.into_owned()
					},
				);

			let content = self.read_selected_file_content();

			if self
				.copy_path_popup
				.open(relative_path, absolute_path, content)
				.is_err()
			{
				self.queue.push(InternalEvent::ShowErrorMsg(
					strings::POPUP_FAIL_COPY.to_string(),
				));
			}
		}
	}

	fn read_selected_file_content(&self) -> Option<String> {
		self.tree.selected_file()?;

		let file = self.selected_file_path_with_prefix()?;
		let path = Path::new(&file);
		let files = self.files.as_ref()?;
		let tree_file = files.iter().find(|f| f.path == path)?;
		tree_file_content(&self.repo.borrow(), tree_file).ok()
	}

	fn selection_changed(&mut self) {
		//TODO: retrieve TreeFile from tree datastructure
		if let Some(file) = self.selected_file_path_with_prefix() {
			if let Some(files) = &self.files {
				let path = Path::new(&file);
				if let Some(item) =
					files.iter().find(|f| f.path == path)
				{
					if let Ok(path) = path.strip_prefix("./") {
						return self.current_file.load_file(
							path.to_string_lossy().to_string(),
							item,
						);
					}
				}
				self.current_file.clear();
			}
		} else if self.tree.selected_item().is_some() {
			// No selected file but a selected item => the focus is on a
			// directory. Show a directory listing instead of leaving the
			// previous file's content on screen.
			self.load_directory_listing();
		} else {
			self.current_file.clear();
		}
	}

	/// Show a colored directory listing (`eza`, falling back to `ls`) for
	/// the currently focused folder. Lists the working tree, not the
	/// revision's tree.
	fn load_directory_listing(&mut self) {
		let Some(dir) = self.selected_item_path() else {
			self.current_file.clear();
			return;
		};

		let Ok(work_dir) = repo_work_dir(&self.repo.borrow()) else {
			self.current_file.clear();
			return;
		};

		match ui::try_dir_listing(
			Path::new(&work_dir),
			&dir,
			self.options.borrow().preview_tree_depth(),
		) {
			Some(content) => {
				self.current_file.load_text(dir.clone(), content);
			}
			None => self.current_file.load_text(
				dir,
				ui::SyntaxText::from_ansi(
					vec![Line::styled(
						"could not list directory (no eza/ls, or path not in working tree)".to_string(),
						self.theme.text(false, false),
					)],
					PathBuf::from("directory"),
				),
			),
		}
	}

	fn draw_tree(&self, f: &mut Frame, area: Rect) -> Result<()> {
		let tree_height = usize::from(area.height.saturating_sub(2));
		let tree_width = usize::from(area.width);

		self.tree.window_height.set(Some(tree_height));

		self.tree.visual_selection().map_or_else(
			|| {
				self.scroll.reset();
			},
			|selection| {
				self.scroll.update(
					selection.index,
					selection.count,
					tree_height,
				);
			},
		);

		let items = self
			.tree
			.iterate(self.scroll.get_top(), tree_height)
			.map(|(item, selected)| {
				Self::tree_item_to_span(
					item,
					&self.theme,
					tree_width,
					selected,
				)
			});

		let is_tree_focused = matches!(self.focus, Focus::Tree);

		let title = self.title_within(tree_width)?;
		let block = Block::default()
			.title(Span::styled(
				title,
				self.theme.title(is_tree_focused),
			))
			.borders(Borders::ALL)
			.border_style(self.theme.block(is_tree_focused));

		if self.files.is_some() {
			ui::draw_list_block(f, area, block, items);
		} else {
			ui::draw_list_block(
				f,
				area,
				block,
				vec![Span::styled(
					Cow::from(strings::loading_text(
						&self.key_config,
					)),
					self.theme.text(false, false),
				)]
				.into_iter(),
			);
		}

		if is_tree_focused {
			self.scroll.draw(f, area, &self.theme);
		}

		Ok(())
	}

	fn title_within(&self, tree_width: usize) -> Result<String> {
		let mut title = String::from("Files at");
		let message = self.revision.as_ref().and_then(|c| {
			let _ignore =
				write!(title, " {{{}}}", c.id.get_short_string());

			c.message.lines().next()
		});

		if let Some(message) = message {
			const ELLIPSIS: char = '\u{2026}'; // …

			let available = tree_width
				.saturating_sub(title.width())
				.saturating_sub(
					2 /* frame end corners */ + 1 /* space */ + 2, /* square brackets */
				);

			if message.width() <= available {
				write!(title, " [{message}]")?;
			} else if available > 1 {
				write!(
					title,
					" [{}{}]",
					message.unicode_truncate(available - 1).0,
					ELLIPSIS
				)?;
			} else {
				title.push(ELLIPSIS);
			}
		}

		Ok(title)
	}

	fn request_files(&self, commit: CommitId) {
		self.async_treefiles.spawn(AsyncTreeFilesJob::new(
			self.repo.borrow().clone(),
			commit,
		));
	}
}

impl DrawableComponent for RevisionFilesComponent {
	fn draw(&self, f: &mut Frame, area: Rect) -> Result<()> {
		if self.is_visible() {
			let left_ratio =
				self.options.borrow().detail_left_ratio();
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

			self.draw_tree(f, chunks[0])?;

			self.current_file.draw(f, chunks[1])?;
		}

		self.copy_path_popup.draw(f, area)?;
		Ok(())
	}
}

impl Component for RevisionFilesComponent {
	fn commands(
		&self,
		out: &mut Vec<CommandInfo>,
		force_all: bool,
	) -> CommandBlocking {
		if !self.is_visible() && !force_all {
			return CommandBlocking::PassingOn;
		}

		let is_tree_focused = matches!(self.focus, Focus::Tree);

		if is_tree_focused || force_all {
			out.push(
				CommandInfo::new(
					strings::commands::blame_file(&self.key_config),
					self.tree.selected_file().is_some(),
					true,
				)
				.order(order::NAV),
			);
			out.push(CommandInfo::new(
				strings::commands::edit_item(&self.key_config),
				self.tree.selected_file().is_some(),
				true,
			));
			out.push(
				CommandInfo::new(
					strings::commands::open_file_history(
						&self.key_config,
					),
					self.tree.selected_item().is_some(),
					true,
				)
				.order(order::RARE_ACTION),
			);
			out.push(
				CommandInfo::new(
					strings::commands::copy_path(&self.key_config),
					self.tree.selected_file().is_some(),
					true,
				)
				.order(order::RARE_ACTION),
			);
			tree_nav_cmds(&self.tree, &self.key_config, out);
		} else {
			self.current_file.commands(out, force_all);
		}

		if self.copy_path_popup.is_visible() {
			return self.copy_path_popup.commands(out, force_all);
		}

		CommandBlocking::PassingOn
	}

	fn event(
		&mut self,
		event: &crossterm::event::Event,
	) -> Result<EventState> {
		if self.copy_path_popup.is_visible() {
			return self.copy_path_popup.event(event);
		}

		if !self.is_visible() {
			return Ok(EventState::NotConsumed);
		}

		if let Event::Key(key) = event {
			let is_tree_focused = matches!(self.focus, Focus::Tree);
			if is_tree_focused
				&& tree_nav(&mut self.tree, &self.key_config, key)
			{
				self.selection_changed();
				return Ok(EventState::Consumed);
			} else if key_match(key, self.key_config.keys.blame) {
				if self.blame() {
					self.hide();
					return Ok(EventState::Consumed);
				}
			} else if key_match(
				key,
				self.key_config.keys.file_history,
			) {
				if self.file_history() {
					self.hide();
					return Ok(EventState::Consumed);
				}
			} else if key_match(key, self.key_config.keys.move_right)
			{
				if is_tree_focused {
					self.focus = Focus::File;
					self.current_file.focus(true);
					self.focus(true);
					return Ok(EventState::Consumed);
				}
			} else if key_match(key, self.key_config.keys.move_left) {
				if !is_tree_focused {
					self.focus = Focus::Tree;
					self.current_file.focus(false);
					self.focus(false);
					return Ok(EventState::Consumed);
				}
			} else if key_match(key, self.key_config.keys.file_find) {
				if is_tree_focused {
					self.open_finder();
					return Ok(EventState::Consumed);
				}
			} else if key_match(key, self.key_config.keys.edit_file) {
				if let Some(file) =
					self.selected_file_path_with_prefix()
				{
					//Note: switch to status tab so its clear we are
					// not altering a file inside a revision here
					self.queue.push(InternalEvent::TabSwitchStatus);
					self.queue.push(
						InternalEvent::OpenExternalEditor(Some(file)),
					);
					return Ok(EventState::Consumed);
				}
			} else if key_match(key, self.key_config.keys.copy)
				|| key_match(key, self.key_config.keys.copy_path)
			{
				self.open_copy_path_popup();
				return Ok(EventState::Consumed);
			} else if !is_tree_focused {
				return self.current_file.event(event);
			}
		}

		Ok(EventState::NotConsumed)
	}

	fn hide(&mut self) {
		self.visible = false;
	}

	fn is_visible(&self) -> bool {
		self.visible
	}

	fn show(&mut self) -> Result<()> {
		self.visible = true;
		self.refresh_files()?;
		Ok(())
	}
}

//TODO: reuse for other tree usages
fn tree_nav_cmds(
	tree: &FileTree,
	key_config: &SharedKeyConfig,
	out: &mut Vec<CommandInfo>,
) {
	out.push(
		CommandInfo::new(
			strings::commands::navigate_tree(key_config),
			!tree.is_empty(),
			true,
		)
		.order(order::NAV),
	);
}

//TODO: reuse for other tree usages
fn tree_nav(
	tree: &mut FileTree,
	key_config: &SharedKeyConfig,
	key: &crossterm::event::KeyEvent,
) -> bool {
	if let Some(common_nav) = common_nav(key, key_config) {
		tree.move_selection(common_nav)
	} else if key_match(key, key_config.keys.tree_collapse_recursive)
	{
		tree.collapse_recursive();
		true
	} else if key_match(key, key_config.keys.tree_expand_recursive) {
		tree.expand_recursive();
		true
	} else {
		false
	}
}
