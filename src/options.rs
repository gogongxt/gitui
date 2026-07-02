use crate::{args::get_app_config_path, components::DiffMode};
use anyhow::Result;
use asyncgit::sync::{
	diff::DiffOptions, repo_dir, RepoPathRef,
	ShowUntrackedFilesConfig,
};
use ron::{
	de::from_bytes,
	ser::{to_string_pretty, PrettyConfig},
};
use serde::{Deserialize, Serialize};
use std::{
	cell::RefCell,
	fs::File,
	io::{Read, Write},
	path::PathBuf,
	rc::Rc,
};

/// Global config options loaded from ~/.config/gitui/config.ron
#[derive(Default, Clone, Serialize, Deserialize)]
#[allow(clippy::struct_field_names)]
struct GlobalOptions {
	pub status_left_ratio: Option<u16>,
	pub log_left_ratio: Option<u16>,
	pub detail_left_ratio: Option<u16>,
}

#[derive(Default, Clone, Serialize, Deserialize)]
struct OptionsData {
	pub tab: usize,
	pub diff: DiffOptions,
	pub status_show_untracked: Option<ShowUntrackedFilesConfig>,
	pub commit_msgs: Vec<String>,
	pub diff_mode: DiffMode,
	pub commit_draft: Option<String>,
	pub commit_draft_cursor: Option<(u16, u16)>,
}

const COMMIT_MSG_HISTORY_LENGTH: usize = 20;

#[derive(Clone)]
pub struct Options {
	repo: RepoPathRef,
	data: OptionsData,
}

#[cfg(test)]
impl Options {
	pub fn test_env() -> Self {
		use asyncgit::sync::RepoPath;
		Self {
			repo: RefCell::new(RepoPath::Path(Default::default())),
			data: Default::default(),
		}
	}
}

pub type SharedOptions = Rc<RefCell<Options>>;

impl Options {
	pub fn new(repo: RepoPathRef) -> SharedOptions {
		Rc::new(RefCell::new(Self {
			data: Self::read(&repo).unwrap_or_default(),
			repo,
		}))
	}

	pub fn set_current_tab(&mut self, tab: usize) {
		self.data.tab = tab;
		self.save();
	}

	pub const fn current_tab(&self) -> usize {
		self.data.tab
	}

	pub const fn diff_options(&self) -> DiffOptions {
		self.data.diff
	}

	pub const fn status_show_untracked(
		&self,
	) -> Option<ShowUntrackedFilesConfig> {
		self.data.status_show_untracked
	}

	pub fn set_status_show_untracked(
		&mut self,
		value: Option<ShowUntrackedFilesConfig>,
	) {
		self.data.status_show_untracked = value;
		self.save();
	}

	pub fn diff_context_change(&mut self, increase: bool) {
		self.data.diff.context = if increase {
			self.data.diff.context.saturating_add(1)
		} else {
			self.data.diff.context.saturating_sub(1)
		};

		self.save();
	}

	pub fn diff_hunk_lines_change(&mut self, increase: bool) {
		self.data.diff.interhunk_lines = if increase {
			self.data.diff.interhunk_lines.saturating_add(1)
		} else {
			self.data.diff.interhunk_lines.saturating_sub(1)
		};

		self.save();
	}

	pub fn diff_toggle_whitespace(&mut self) {
		self.data.diff.ignore_whitespace =
			!self.data.diff.ignore_whitespace;

		self.save();
	}

	pub const fn diff_mode(&self) -> DiffMode {
		self.data.diff_mode
	}

	#[allow(clippy::unused_self)]
	pub fn status_left_ratio(&self) -> u16 {
		Self::read_global()
			.ok()
			.and_then(|g| g.status_left_ratio)
			.map_or(50, |r| r.clamp(10, 90))
	}

	#[allow(clippy::unused_self)]
	pub fn log_left_ratio(&self) -> u16 {
		Self::read_global()
			.ok()
			.and_then(|g| g.log_left_ratio)
			.map_or(60, |r| r.clamp(10, 90))
	}

	#[allow(clippy::unused_self)]
	pub fn detail_left_ratio(&self) -> u16 {
		Self::read_global()
			.ok()
			.and_then(|g| g.detail_left_ratio)
			.map_or(50, |r| r.clamp(10, 90))
	}

	pub fn set_diff_mode(&mut self, mode: DiffMode) {
		self.data.diff_mode = mode;
		self.save();
	}

	pub fn add_commit_msg(&mut self, msg: &str) {
		self.data.commit_msgs.push(msg.to_owned());
		while self.data.commit_msgs.len() > COMMIT_MSG_HISTORY_LENGTH
		{
			self.data.commit_msgs.remove(0);
		}
		self.save();
	}

	pub const fn has_commit_msg_history(&self) -> bool {
		!self.data.commit_msgs.is_empty()
	}

	pub fn commit_msg(&self, idx: usize) -> Option<String> {
		if self.data.commit_msgs.is_empty() {
			None
		} else {
			let entries = self.data.commit_msgs.len();
			let mut index = idx;

			while index >= entries {
				index -= entries;
			}

			index = entries.saturating_sub(1) - index;

			Some(self.data.commit_msgs[index].clone())
		}
	}

	pub const fn commit_draft(&self) -> Option<&String> {
		self.data.commit_draft.as_ref()
	}

	pub const fn commit_draft_cursor(&self) -> Option<(u16, u16)> {
		self.data.commit_draft_cursor
	}

	/// Set the draft message. When `msg` is `None` or empty, both the
	/// draft text and its saved cursor are cleared — they are treated
	/// as a single logical unit so a stale cursor can never outlive its
	/// text.
	pub fn set_commit_draft(&mut self, msg: Option<String>) {
		let msg = msg.filter(|s| !s.is_empty());
		if msg.is_none() {
			self.data.commit_draft_cursor = None;
		}
		self.data.commit_draft = msg;
		self.save();
	}

	/// Set the draft cursor. Should only be called alongside a
	/// non-empty `set_commit_draft`; clearing the draft text already
	/// clears the cursor.
	pub fn set_commit_draft_cursor(
		&mut self,
		cursor: Option<(u16, u16)>,
	) {
		if self.data.commit_draft.is_none() {
			return;
		}
		self.data.commit_draft_cursor = cursor;
		self.save();
	}

	fn save(&self) {
		if let Err(e) = self.save_failable() {
			log::error!("options save error: {e}");
		}
	}

	fn read(repo: &RepoPathRef) -> Result<OptionsData> {
		let dir = Self::options_file(repo)?;

		let mut f = File::open(dir)?;
		let mut buffer = Vec::new();
		f.read_to_end(&mut buffer)?;
		Ok(from_bytes(&buffer)?)
	}

	fn read_global() -> Result<GlobalOptions> {
		let path = get_app_config_path()?.join("config.ron");
		let mut f = File::open(path)?;
		let mut buffer = Vec::new();
		f.read_to_end(&mut buffer)?;
		Ok(from_bytes(&buffer)?)
	}

	fn save_failable(&self) -> Result<()> {
		let dir = Self::options_file(&self.repo)?;

		let mut file = File::create(dir)?;
		let data =
			to_string_pretty(&self.data, PrettyConfig::default())?;
		file.write_all(data.as_bytes())?;

		Ok(())
	}

	fn options_file(repo: &RepoPathRef) -> Result<PathBuf> {
		let dir = repo_dir(&repo.borrow())?;
		let dir = dir.join("gitui");
		Ok(dir)
	}
}

#[cfg(test)]
mod tests {
	use super::{Options, OptionsData};
	use crate::app::Environment;
	use asyncgit::sync::RepoPath;
	use std::cell::RefCell;
	use tempfile::TempDir;

	fn init_repo(td: &TempDir) {
		for args in [
			&["init", "-q"][..],
			&["config", "user.email", "t@t.t"][..],
			&["config", "user.name", "t"][..],
		] {
			let status = std::process::Command::new("git")
				.args(args)
				.current_dir(td.path())
				.status()
				.unwrap();
			assert!(status.success(), "git {:?}", args);
		}
	}

	#[test]
	fn set_commit_draft_normalizes_empty_to_none() {
		let mut opts = Options::test_env();
		opts.set_commit_draft(Some("wip: draft".to_string()));
		assert_eq!(
			opts.commit_draft(),
			Some(&"wip: draft".to_string())
		);

		opts.set_commit_draft(Some(String::new()));
		assert_eq!(opts.commit_draft(), None);

		opts.set_commit_draft(None);
		assert_eq!(opts.commit_draft(), None);
	}

	/// Clearing the draft text must also clear the saved cursor, so a
	/// stale cursor can never outlive its text.
	#[test]
	fn clearing_draft_clears_cursor() {
		let mut opts = Options::test_env();
		opts.set_commit_draft(Some("wip: draft".to_string()));
		opts.set_commit_draft_cursor(Some((2, 5)));
		assert_eq!(opts.commit_draft_cursor(), Some((2, 5)));

		// clearing via empty string clears the cursor too
		opts.set_commit_draft(Some(String::new()));
		assert_eq!(opts.commit_draft(), None);
		assert_eq!(opts.commit_draft_cursor(), None);

		// setting a cursor without a draft text is a no-op
		opts.set_commit_draft_cursor(Some((1, 1)));
		assert_eq!(opts.commit_draft_cursor(), None);
	}

	/// Options files written by older gitui versions lack the
	/// `commit_draft` field. Deserialization must treat it as `None`
	/// (via `#[derive(Default)]`), not error.
	#[test]
	fn commit_draft_absent_in_old_options_file_defaults_to_none() {
		let old_ron = r#"(
            tab: 1,
            diff: (
                context: 3,
                interhunk_lines: 0,
                ignore_whitespace: false,
            ),
            status_show_untracked: None,
            commit_msgs: [],
            diff_mode: Unified,
        )"#;
		let data: OptionsData = ron::from_str(old_ron).unwrap();
		assert_eq!(data.commit_draft, None);
		assert_eq!(data.commit_draft_cursor, None);
		assert_eq!(data.tab, 1);
	}

	/// Draft should round-trip through the on-disk options file in a
	/// real repo, so a fresh `Options` instance picks it up. This
	/// covers the "reopen gitui after restart" path — both text and
	/// saved cursor position.
	#[test]
	fn commit_draft_persists_to_disk() {
		let td = TempDir::new().unwrap();
		init_repo(&td);

		let repo =
			RefCell::new(RepoPath::Path(td.path().to_path_buf()));

		// first instance: set a draft + cursor, which writes to disk
		let opts1 = Options::new(repo);
		opts1
			.borrow_mut()
			.set_commit_draft(Some("wip: from disk".to_string()));
		opts1.borrow_mut().set_commit_draft_cursor(Some((1, 3)));
		drop(opts1);

		// second instance over the same repo: should read both back
		let opts2 = Options::new(RefCell::new(RepoPath::Path(
			td.path().to_path_buf(),
		)));
		assert_eq!(
			opts2.borrow().commit_draft(),
			Some(&"wip: from disk".to_string()),
		);
		assert_eq!(
			opts2.borrow().commit_draft_cursor(),
			Some((1, 3))
		);

		// clearing also persists (and clears the cursor)
		opts2.borrow_mut().set_commit_draft(None);
		drop(opts2);

		let opts3 = Options::new(RefCell::new(RepoPath::Path(
			td.path().to_path_buf(),
		)));
		assert_eq!(opts3.borrow().commit_draft(), None);
		assert_eq!(opts3.borrow().commit_draft_cursor(), None);
	}

	// The two tests below exercise `CommitPopup` (in `popups/commit.rs`)
	// rather than `Options` directly. They live here because they verify
	// the draft persistence round-trip — the contract `Options` exposes
	// to the popup — and `commit.rs` has no `mod tests` of its own.
	// `init_repo` above is reused by both.

	/// Smoke test that `CommitPopup::open` restores a previously-saved
	/// draft into the input field in Normal mode. Verifies the wiring
	/// (draft → input text) without driving the full TUI.
	#[test]
	fn commit_popup_open_restores_draft() {
		use crate::popups::CommitPopup;

		let td = TempDir::new().unwrap();
		init_repo(&td);

		let mut env = Environment::test_env();
		env.repo =
			RefCell::new(RepoPath::Path(td.path().to_path_buf()));

		// seed a draft + cursor on disk via the options shared with the popup
		env.options
			.borrow_mut()
			.set_commit_draft(Some("wip: popup restore".to_string()));
		env.options
			.borrow_mut()
			.set_commit_draft_cursor(Some((0, 5)));

		let mut popup = CommitPopup::new(&env);
		popup.open(None).unwrap();

		assert_eq!(
			popup.get_text(),
			"wip: popup restore",
			"open() should restore the persisted draft in Normal mode"
		);
		assert_eq!(
			popup.cursor(),
			Some((0, 5)),
			"open() should restore the saved cursor position"
		);
	}

	/// Within a single session, c-q close preserves the in-memory
	/// draft text but destroys the textarea (resetting cursor to
	/// 0,0 on re-show). Reopening should restore the cursor that was
	/// cached at hide time — not just the text.
	#[test]
	fn commit_popup_reopen_in_same_session_restores_cursor() {
		use crate::popups::CommitPopup;

		let td = TempDir::new().unwrap();
		init_repo(&td);

		let mut env = Environment::test_env();
		env.repo =
			RefCell::new(RepoPath::Path(td.path().to_path_buf()));

		let mut popup = CommitPopup::new(&env);
		popup.open(None).unwrap();
		// type some text and move cursor to (0, 3)
		popup.text_input_mut().set_text("wip".to_string());
		popup.text_input_mut().set_cursor(0, 3);
		assert_eq!(popup.cursor(), Some((0, 3)));

		// simulate c-q: persist draft (saves text + cursor), then the
		// textarea is hidden. CommitPopup::persist_draft reads the
		// cursor via input.cursor() which falls back to the cache.
		popup.persist_draft();
		popup.hide_for_test();

		// reopen in the same session — input still has text, so we
		// hit the "same-session reopen" path. Cursor must come back.
		popup.open(None).unwrap();
		assert_eq!(popup.get_text(), "wip");
		assert_eq!(
			popup.cursor(),
			Some((0, 3)),
			"reopen in same session should restore cached cursor"
		);
	}
}
