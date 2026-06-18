use super::{
	commit_details::get_author_of_commit,
	commit_files::get_commit_diff, CommitId,
};
use crate::error::Result;
use bitflags::bitflags;
use fuzzy_matcher::FuzzyMatcher;
use git2::{Diff, Repository};
use std::{
	collections::HashMap,
	path::{Path, PathBuf},
	sync::{
		atomic::{AtomicU64, Ordering},
		Arc,
	},
};

///
pub type SharedCommitFilterFn = Arc<
	Box<dyn Fn(&Repository, &CommitId) -> Result<bool> + Send + Sync>,
>;

/// Cumulative time spent inside the per-commit filter, broken down by phase.
/// Shared across threads via `Arc`, updated with `Relaxed` ordering.
#[derive(Default)]
pub struct FilterTimings {
	/// Time spent in `repo.mailmap()` across all calls (microseconds).
	pub mailmap_us: AtomicU64,
	/// Time spent in `repo.find_commit()` across all calls (microseconds).
	pub find_commit_us: AtomicU64,
	/// Time spent in `get_commit_diff()` across all calls (microseconds).
	pub diff_us: AtomicU64,
	/// Time spent resolving authors with mailmap across all calls (microseconds).
	pub author_us: AtomicU64,
	/// Wall-clock time of the whole filter call accumulated across calls (microseconds).
	pub other_us: AtomicU64,
	/// Number of times the filter closure was invoked.
	pub calls: AtomicU64,
}

impl FilterTimings {
	#[allow(clippy::unused_self)]
	fn add(&self, counter: &AtomicU64, us: u64) {
		counter.fetch_add(us, Ordering::Relaxed);
	}
}

// Per-thread cache of `Mailmap` objects, keyed by the repository's workdir
// path. `git2::Mailmap` is `!Send`, so it must live in thread-local storage.
//
// `repo.mailmap()` stat-opens several paths (`.mailmap`, repo config, etc.)
// on every call; without caching this dominates search latency on NFS.
// The mailmap is independent of the commit being filtered, so one per repo
// per thread is sufficient. `None` means the repo has no mailmap (or reading
// it failed), so author resolution falls back to the raw commit author.
thread_local! {
	static MAILMAP_CACHE:
		std::cell::RefCell<HashMap<PathBuf, Option<git2::Mailmap>>> =
		std::cell::RefCell::new(HashMap::new());
}

///
pub fn diff_contains_file(file_path: String) -> SharedCommitFilterFn {
	Arc::new(Box::new(
		move |repo: &Repository,
		      commit_id: &CommitId|
		      -> Result<bool> {
			let diff = get_commit_diff(
				repo,
				*commit_id,
				Some(file_path.clone()),
				None,
				None,
			)?;

			let contains_file = diff.deltas().len() > 0;

			Ok(contains_file)
		},
	))
}

bitflags! {
	///
	#[derive(Debug, Clone, Copy)]
	pub struct SearchFields: u32 {
		///
		const MESSAGE_SUMMARY = 1 << 0;
		///
		const MESSAGE_BODY = 1 << 1;
		///
		const FILENAMES = 1 << 2;
		///
		const AUTHORS = 1 << 3;
		///
		const COMMIT_HASHES = 1 << 4;
		//TODO:
		// ///
		// const DATES = 1 << 5;
		// ///
		// const DIFFS = 1 << 6;
	}
}

impl Default for SearchFields {
	fn default() -> Self {
		Self::MESSAGE_SUMMARY
	}
}

bitflags! {
	///
	#[derive(Debug, Clone, Copy)]
	pub struct SearchOptions: u32 {
		///
		const CASE_SENSITIVE = 1 << 0;
		///
		const FUZZY_SEARCH = 1 << 1;
	}
}

impl Default for SearchOptions {
	fn default() -> Self {
		Self::empty()
	}
}

///
#[derive(Default, Debug, Clone)]
pub struct LogFilterSearchOptions {
	///
	pub search_pattern: String,
	///
	pub fields: SearchFields,
	///
	pub options: SearchOptions,
}

///
#[derive(Default)]
pub struct LogFilterSearch {
	///
	pub matcher: fuzzy_matcher::skim::SkimMatcherV2,
	///
	pub options: LogFilterSearchOptions,
}

impl LogFilterSearch {
	///
	pub fn new(options: LogFilterSearchOptions) -> Self {
		let mut options = options;
		if !options.options.contains(SearchOptions::CASE_SENSITIVE) {
			options.search_pattern =
				options.search_pattern.to_lowercase();
		}
		Self {
			matcher: fuzzy_matcher::skim::SkimMatcherV2::default(),
			options,
		}
	}

	fn match_diff(&self, diff: &Diff<'_>) -> bool {
		diff.deltas().any(|delta| {
			if delta
				.new_file()
				.path()
				.and_then(|file| file.as_os_str().to_str())
				.is_some_and(|file| self.match_text(file))
			{
				return true;
			}

			delta
				.old_file()
				.path()
				.and_then(|file| file.as_os_str().to_str())
				.is_some_and(|file| self.match_text(file))
		})
	}

	///
	pub fn match_text(&self, text: &str) -> bool {
		if self.options.options.contains(SearchOptions::FUZZY_SEARCH)
		{
			self.matcher
				.fuzzy_match(
					text,
					self.options.search_pattern.as_str(),
				)
				.is_some()
		} else if self
			.options
			.options
			.contains(SearchOptions::CASE_SENSITIVE)
		{
			text.contains(self.options.search_pattern.as_str())
		} else {
			text.to_lowercase()
				.contains(self.options.search_pattern.as_str())
		}
	}
}

///
#[allow(clippy::cast_possible_truncation, clippy::too_many_lines)]
pub fn filter_commit_by_search(
	filter: LogFilterSearch,
	timings: Arc<FilterTimings>,
) -> SharedCommitFilterFn {
	Arc::new(Box::new(
		move |repo: &Repository,
		      commit_id: &CommitId|
		      -> Result<bool> {
			let call_start = std::time::Instant::now();
			timings.calls.fetch_add(1, Ordering::Relaxed);

			let t0 = std::time::Instant::now();
			let commit = repo.find_commit((*commit_id).into())?;
			timings.add(
				&timings.find_commit_us,
				t0.elapsed().as_micros() as u64,
			);

			let msg_summary_match = filter
				.options
				.fields
				.contains(SearchFields::MESSAGE_SUMMARY)
				.then(|| {
					commit.summary().map(|msg| filter.match_text(msg))
				})
				.flatten()
				.unwrap_or_default();

			let msg_body_match = filter
				.options
				.fields
				.contains(SearchFields::MESSAGE_BODY)
				.then(|| {
					commit.body().map(|msg| filter.match_text(msg))
				})
				.flatten()
				.unwrap_or_default();

			let file_match = filter
				.options
				.fields
				.contains(SearchFields::FILENAMES)
				.then(|| {
					let t = std::time::Instant::now();
					let r = get_commit_diff(
						repo, *commit_id, None, None, None,
					)
					.ok();
					timings.add(
						&timings.diff_us,
						t.elapsed().as_micros() as u64,
					);
					r
				})
				.flatten()
				.is_some_and(|diff| filter.match_diff(&diff));

			let authors_match = if filter
				.options
				.fields
				.contains(SearchFields::AUTHORS)
			{
				let commit_ref = &commit;
				let filter_ref = &filter;
				let mailmap_t = std::time::Instant::now();
				// Mailmap is repo-scoped, not commit-scoped. Cache it per
				// thread keyed by workdir path so we only stat the mailmap
				// sources once per repo per thread, instead of once per
				// commit. `git2::Mailmap` is `!Send` and un-Cloneable, so
				// the author resolution must happen inside the `with` block
				// while we hold a borrowed reference to the cached mailmap.
				let r = MAILMAP_CACHE.with(|cache| {
					let mut cache = cache.borrow_mut();
					let key = repo
						.workdir()
						.map(Path::to_path_buf)
						.unwrap_or_default();
					let mailmap = cache
						.entry(key)
						.or_insert_with(|| repo.mailmap().ok());
					timings.add(
						&timings.mailmap_us,
						mailmap_t.elapsed().as_micros() as u64,
					);
					let author_t = std::time::Instant::now();
					let author = mailmap.as_ref().map_or_else(
						|| commit_ref.author(),
						|mm| get_author_of_commit(commit_ref, mm),
					);
					let matched = [author.email(), author.name()]
						.iter()
						.any(|opt_haystack| {
							opt_haystack.is_some_and(|haystack| {
								filter_ref.match_text(haystack)
							})
						});
					timings.add(
						&timings.author_us,
						author_t.elapsed().as_micros() as u64,
					);
					matched
				});
				r
			} else {
				false
			};

			let commit_hash_match = if filter
				.options
				.fields
				.contains(SearchFields::COMMIT_HASHES)
			{
				// Search in both short and full hash
				let short_hash = commit_id.to_string();
				let full_hash = commit.id().to_string();
				filter.match_text(&short_hash)
					|| filter.match_text(&full_hash)
			} else {
				false
			};

			timings.add(
				&timings.other_us,
				call_start.elapsed().as_micros() as u64,
			);

			Ok(msg_summary_match
				|| msg_body_match
				|| file_match
				|| authors_match
				|| commit_hash_match)
		},
	))
}
