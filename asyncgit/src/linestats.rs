use crate::{
	error::Result,
	hash,
	sync::{self, diff::DiffOptions, RepoPath},
	AsyncGitNotification,
};
use crossbeam_channel::Sender;
use std::{
	hash::Hash,
	sync::{
		atomic::{AtomicU64, AtomicUsize, Ordering},
		Arc, Mutex,
	},
};

/// (added, deleted) line counts for both panes, computed together
/// in a single background job so the UI thread never blocks on diff
/// stats. See [`AsyncLineStats`].
#[derive(Default, Hash, Clone, Copy, PartialEq, Eq)]
pub struct LineStats {
	/// total (added, deleted) across all staged files
	pub staged: (usize, usize),
	/// total (added, deleted) across unstaged tracked files
	/// (untracked files excluded)
	pub unstaged: (usize, usize),
}

struct Request<R, A>(R, Option<A>);

/// Async counterpart of [`sync::diff::get_staged_line_stats`] /
/// [`sync::diff::get_unstaged_line_stats`].
///
/// Both counts are computed together in one background `rayon` job so
/// the per-status-refresh cost stays off the UI thread. The request is
/// keyed by [`DiffOptions`] (plus a generation counter, like
/// [`crate::AsyncStatus`]), so toggling e.g. `ignore_whitespace` raises
/// a fresh request and the displayed `(+.. -..)` follows.
pub struct AsyncLineStats {
	current: Arc<Mutex<Request<u64, LineStats>>>,
	last: Arc<Mutex<Option<LineStats>>>,
	sender: Sender<AsyncGitNotification>,
	pending: Arc<AtomicUsize>,
	repo: RepoPath,
	/// Counter that increments after each completed fetch, so a stale
	/// in-flight result never satisfies a newer request (mirrors
	/// `AsyncStatus::generation`).
	generation: Arc<AtomicU64>,
}

impl AsyncLineStats {
	///
	pub fn new(
		repo: RepoPath,
		sender: &Sender<AsyncGitNotification>,
	) -> Self {
		Self {
			repo,
			current: Arc::new(Mutex::new(Request(0, None))),
			last: Arc::new(Mutex::new(None)),
			sender: sender.clone(),
			pending: Arc::new(AtomicUsize::new(0)),
			generation: Arc::new(AtomicU64::new(0)),
		}
	}

	/// Most recent computed stats, if any. Cheap: one mutex lock +
	/// copy. Safe to call every frame from the UI thread.
	pub fn last(&self) -> Result<Option<LineStats>> {
		Ok(*self.last.lock()?)
	}

	///
	pub fn is_pending(&self) -> bool {
		self.pending.load(Ordering::Relaxed) > 0
	}

	/// Request a (re)compute. Returns the cached result immediately if
	/// the params (options + generation) are unchanged; otherwise kicks
	/// off a background job and returns `None`. Idempotent under
	/// repeated identical requests.
	pub fn fetch(
		&self,
		options: DiffOptions,
	) -> Result<Option<LineStats>> {
		if self.is_pending() {
			log::trace!("linestats request blocked, still pending");
			return Ok(None);
		}

		let generation = self.generation.load(Ordering::Relaxed);
		let hash_request = hash(&(options, generation));

		{
			let mut current = self.current.lock()?;

			if current.0 == hash_request {
				return Ok(current.1);
			}

			current.0 = hash_request;
			current.1 = None;
		}

		let arc_current = Arc::clone(&self.current);
		let arc_last = Arc::clone(&self.last);
		let arc_generation = Arc::clone(&self.generation);
		let sender = self.sender.clone();
		let arc_pending = Arc::clone(&self.pending);
		let repo = self.repo.clone();

		self.pending.fetch_add(1, Ordering::Relaxed);

		rayon_core::spawn(move || {
			let res = Self::fetch_helper(
				&repo,
				options,
				hash_request,
				&arc_current,
				&arc_last,
			);

			// `fetch_helper` already published under the `current`
			// guard (honoring `hash_request`), so a late result can't
			// clobber a newer request. We only need the error log
			// here; no further writes.
			if let Err(e) = res {
				log::error!("linestats fetch_helper: {e}");
			}

			arc_generation.fetch_add(1, Ordering::Relaxed);
			arc_pending.fetch_sub(1, Ordering::Relaxed);

			sender
				.send(AsyncGitNotification::LineStats)
				.expect("error sending linestats");
		});

		Ok(None)
	}

	fn fetch_helper(
		repo: &RepoPath,
		options: DiffOptions,
		hash_request: u64,
		arc_current: &Arc<Mutex<Request<u64, LineStats>>>,
		arc_last: &Arc<Mutex<Option<LineStats>>>,
	) -> Result<LineStats> {
		let staged =
			sync::diff::get_staged_line_stats(repo, Some(options))?;
		let unstaged =
			sync::diff::get_unstaged_line_stats(repo, Some(options))?;

		let stats = LineStats { staged, unstaged };

		log::trace!("linestats fetched: {hash_request}");

		{
			let mut current = arc_current.lock()?;
			if current.0 == hash_request {
				current.1 = Some(stats);
			}
		}

		{
			let mut last = arc_last.lock()?;
			*last = Some(stats);
		}

		Ok(stats)
	}
}
