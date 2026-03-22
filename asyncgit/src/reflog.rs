use crate::{
	error::Result,
	sync::{get_reflog, ReflogEntry},
	AsyncGitNotification,
};
use crossbeam_channel::Sender;
use std::{
	path::PathBuf,
	sync::{
		atomic::{AtomicBool, Ordering},
		Arc, Mutex,
	},
	thread,
	time::{Duration, Instant, SystemTime},
};

///
#[derive(PartialEq, Eq, Debug, Clone)]
pub enum FetchStatus {
	/// previous fetch still running
	Pending,
	/// no change expected
	NoChange,
	/// new walk was started
	Started,
}

///
pub struct AsyncReflogResult {
	///
	pub entries: Vec<ReflogEntry>,
	#[allow(dead_code)]
	///
	pub duration: Duration,
}

///
pub struct AsyncReflog {
	current: Arc<Mutex<AsyncReflogResult>>,
	/// Track modification time of .git/logs/HEAD to detect changes
	reflog_mtime: Arc<Mutex<Option<SystemTime>>>,
	sender: Sender<AsyncGitNotification>,
	pending: Arc<AtomicBool>,
	repo: crate::sync::RepoPath,
}

impl AsyncReflog {
	///
	pub fn new(
		repo: crate::sync::RepoPath,
		sender: &Sender<AsyncGitNotification>,
	) -> Self {
		Self {
			repo,
			current: Arc::new(Mutex::new(AsyncReflogResult {
				entries: Vec::new(),
				duration: Duration::default(),
			})),
			reflog_mtime: Arc::new(Mutex::new(None)),
			sender: sender.clone(),
			pending: Arc::new(AtomicBool::new(false)),
		}
	}

	///
	pub fn count(&self) -> Result<usize> {
		Ok(self.current.lock()?.entries.len())
	}

	///
	pub fn get_items(&self) -> Result<Vec<ReflogEntry>> {
		Ok(self.current.lock()?.entries.clone())
	}

	///
	pub fn is_pending(&self) -> bool {
		self.pending.load(Ordering::Relaxed)
	}

	/// Get the path to the reflog file
	fn reflog_path(&self) -> PathBuf {
		// Try to find .git/logs/HEAD or worktree-specific path
		let (base_path, git_dir) = match &self.repo {
			crate::sync::RepoPath::Path(p) => {
				// Check if .git exists directly or we're inside .git
				let git_dir = if p.join(".git").exists() {
					p.join(".git")
				} else if p.file_name().is_some_and(|n| n == ".git") {
					p.clone()
				} else if p.extension().is_none()
					&& p.join("logs").exists()
				{
					// We're already in .git directory
					p.clone()
				} else {
					p.join(".git")
				};
				(None, git_dir)
			}
			crate::sync::RepoPath::Workdir { gitdir, workdir } => {
				// For worktrees, reflog is in the worktree's gitdir
				// gitdir is the .git/worktrees/<name> directory
				(Some(workdir.clone()), gitdir.clone())
			}
		};

		// For worktrees, check if logs/HEAD exists in worktree git dir
		// For regular repos, use the git_dir
		let git_dir = git_dir
			.canonicalize()
			.unwrap_or_else(|_| git_dir.clone());

		let reflog_path = git_dir.join("logs").join("HEAD");

		// For worktrees, also check if the reflog might be in the main repo
		if !reflog_path.exists() {
			if let Some(base) = base_path {
				// Try the main repo's reflog as fallback
				let main_git_dir = if base.join(".git").is_dir() {
					base.join(".git")
				} else {
					base
				};
				let main_reflog =
					main_git_dir.join("logs").join("HEAD");
				if main_reflog.exists() {
					return main_reflog;
				}
			}
		}

		reflog_path
	}

	/// Check if reflog file has been modified since last fetch
	fn reflog_changed(&self) -> Result<bool> {
		let stored_mtime = *self.reflog_mtime.lock()?;

		// If we have no stored mtime, we need to fetch
		let Some(stored_mtime) = stored_mtime else {
			return Ok(true);
		};

		// Check the modification time of the reflog file
		let reflog_path = self.reflog_path();
		if let Ok(metadata) = std::fs::metadata(&reflog_path) {
			if let Ok(current_mtime) = metadata.modified() {
				return Ok(current_mtime != stored_mtime);
			}
		}

		// If we can't check the file, assume changed to be safe
		Ok(true)
	}

	/// Fetch reflog data, only if changed since last fetch
	/// Similar to `AsyncLog::fetch()` pattern
	#[allow(clippy::cognitive_complexity)]
	pub fn fetch(&self) -> Result<FetchStatus> {
		if self.pending.load(Ordering::Relaxed) {
			return Ok(FetchStatus::Pending);
		}

		// Check if reflog actually changed
		if !self.reflog_changed()? {
			return Ok(FetchStatus::NoChange);
		}

		// Reflog changed, start async fetch
		self.pending.store(true, Ordering::Relaxed);

		let sender = self.sender.clone();
		let current = self.current.clone();
		let reflog_mtime = self.reflog_mtime.clone();
		let pending = self.pending.clone();
		let repo = self.repo.clone();
		let reflog_path = self.reflog_path();

		thread::spawn(move || {
			let start = Instant::now();
			let result = get_reflog(&repo);

			match result {
				Ok(entries) => {
					let duration = start.elapsed();

					// Update the modification time for future change detection
					if let Ok(metadata) =
						std::fs::metadata(&reflog_path)
					{
						if let Ok(mut mtime) = reflog_mtime.lock() {
							*mtime = metadata.modified().ok();
						}
					}

					if let Ok(mut current) = current.lock() {
						*current =
							AsyncReflogResult { entries, duration };
					}

					let _ = sender.send(AsyncGitNotification::Reflog);
				}
				Err(e) => {
					log::error!("failed to get reflog: {e}");
				}
			}

			pending.store(false, Ordering::Relaxed);
		});

		Ok(FetchStatus::Started)
	}

	/// request a refresh, non-blocking
	/// returns the status of a potential prior running request
	#[allow(dead_code)]
	pub fn request(&self) -> Result<FetchStatus> {
		self.fetch()
	}

	/// check if there was a change since last `request()` call
	/// if there was a change, the current data will be updated
	#[allow(dead_code)]
	pub const fn check_for_updates(&self) -> Result<FetchStatus> {
		Ok(FetchStatus::NoChange)
	}
}
