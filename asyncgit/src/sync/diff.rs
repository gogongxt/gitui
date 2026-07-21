//! sync git api for fetching a diff

use super::{
	commit_files::{
		get_commit_diff, get_compare_commits_diff, OldNew,
	},
	utils::{get_head_repo, work_dir},
	CommitId, RepoPath,
};
use crate::{
	error::Error,
	error::Result,
	hash,
	sync::{get_stashes, repository::repo},
};
use easy_cast::Conv;
use git2::{
	Delta, Diff, DiffDelta, DiffFormat, DiffHunk, Patch, Repository,
};
use scopetime::scope_time;
use serde::{Deserialize, Serialize};
use std::{cell::RefCell, fs, path::Path, rc::Rc};

/// type of diff of a single line
#[derive(Copy, Clone, Default, PartialEq, Eq, Hash, Debug)]
pub enum DiffLineType {
	/// just surrounding line, no change
	#[default]
	None,
	/// header of the hunk
	Header,
	/// line added
	Add,
	/// line deleted
	Delete,
}

impl From<git2::DiffLineType> for DiffLineType {
	fn from(line_type: git2::DiffLineType) -> Self {
		match line_type {
			git2::DiffLineType::HunkHeader => Self::Header,
			git2::DiffLineType::DeleteEOFNL
			| git2::DiffLineType::Deletion => Self::Delete,
			git2::DiffLineType::AddEOFNL
			| git2::DiffLineType::Addition => Self::Add,
			_ => Self::None,
		}
	}
}

///
#[derive(Default, Clone, Hash, Debug)]
pub struct DiffLine {
	///
	pub content: Box<str>,
	///
	pub line_type: DiffLineType,
	///
	pub position: DiffLinePosition,
}

///
#[derive(Clone, Copy, Default, Hash, Debug, PartialEq, Eq)]
pub struct DiffLinePosition {
	///
	pub old_lineno: Option<u32>,
	///
	pub new_lineno: Option<u32>,
}

impl PartialEq<&git2::DiffLine<'_>> for DiffLinePosition {
	fn eq(&self, other: &&git2::DiffLine) -> bool {
		other.new_lineno() == self.new_lineno
			&& other.old_lineno() == self.old_lineno
	}
}

impl From<&git2::DiffLine<'_>> for DiffLinePosition {
	fn from(line: &git2::DiffLine<'_>) -> Self {
		Self {
			old_lineno: line.old_lineno(),
			new_lineno: line.new_lineno(),
		}
	}
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Hash)]
pub(crate) struct HunkHeader {
	pub old_start: u32,
	pub old_lines: u32,
	pub new_start: u32,
	pub new_lines: u32,
}

impl From<DiffHunk<'_>> for HunkHeader {
	fn from(h: DiffHunk) -> Self {
		Self {
			old_start: h.old_start(),
			old_lines: h.old_lines(),
			new_start: h.new_start(),
			new_lines: h.new_lines(),
		}
	}
}

/// single diff hunk
#[derive(Default, Clone, Hash, Debug)]
pub struct Hunk {
	/// hash of the hunk header
	pub header_hash: u64,
	/// list of `DiffLine`s
	pub lines: Vec<DiffLine>,
}

/// collection of hunks, sum of all diff lines
#[derive(Default, Clone, Hash, Debug)]
pub struct FileDiff {
	/// list of hunks
	pub hunks: Vec<Hunk>,
	/// lines total summed up over hunks
	pub lines: usize,
	///
	pub untracked: bool,
	/// old and new file size in bytes
	pub sizes: (u64, u64),
	/// size delta in bytes
	pub size_delta: i64,
}

/// see <https://libgit2.org/libgit2/#HEAD/type/git_diff_options>
#[derive(
	Debug, Hash, Clone, Copy, PartialEq, Eq, Serialize, Deserialize,
)]
pub struct DiffOptions {
	/// see <https://libgit2.org/libgit2/#HEAD/type/git_diff_options>
	pub ignore_whitespace: bool,
	/// see <https://libgit2.org/libgit2/#HEAD/type/git_diff_options>
	pub context: u32,
	/// see <https://libgit2.org/libgit2/#HEAD/type/git_diff_options>
	pub interhunk_lines: u32,
}

impl Default for DiffOptions {
	fn default() -> Self {
		Self {
			ignore_whitespace: false,
			context: 3,
			interhunk_lines: 0,
		}
	}
}

pub(crate) fn get_diff_raw<'a>(
	repo: &'a Repository,
	p: &str,
	stage: bool,
	reverse: bool,
	options: Option<DiffOptions>,
) -> Result<Diff<'a>> {
	// scope_time!("get_diff_raw");

	let mut opt = git2::DiffOptions::new();
	if let Some(options) = options {
		opt.context_lines(options.context);
		opt.ignore_whitespace(options.ignore_whitespace);
		opt.interhunk_lines(options.interhunk_lines);
	}
	opt.pathspec(p);
	opt.reverse(reverse);

	let diff = if stage {
		// diff against head
		if let Ok(id) = get_head_repo(repo) {
			let parent = repo.find_commit(id.into())?;

			let tree = parent.tree()?;
			repo.diff_tree_to_index(
				Some(&tree),
				Some(&repo.index()?),
				Some(&mut opt),
			)?
		} else {
			repo.diff_tree_to_index(
				None,
				Some(&repo.index()?),
				Some(&mut opt),
			)?
		}
	} else {
		opt.include_untracked(true);
		opt.recurse_untracked_dirs(true);
		repo.diff_index_to_workdir(None, Some(&mut opt))?
	};

	Ok(diff)
}

/// returns diff of a specific file either in `stage` or workdir
pub fn get_diff(
	repo_path: &RepoPath,
	p: &str,
	stage: bool,
	options: Option<DiffOptions>,
) -> Result<FileDiff> {
	scope_time!("get_diff");

	let repo = repo(repo_path)?;
	let work_dir = work_dir(&repo)?;
	let diff = get_diff_raw(&repo, p, stage, false, options)?;

	raw_diff_to_file_diff(&diff, work_dir)
}

/// total (added, deleted) lines across all staged files.
///
/// This is the *aggregate* counterpart of [`get_diff`] for the whole
/// index: instead of building a full [`FileDiff`] (with all hunks and
/// lines) per file just to count `+`/`-` lines, it runs a single
/// tree→index diff and reads `git2::Diff::stats()`. That avoids the
/// `O(lines)` hunk/line parsing on the UI thread that the old
/// per-file fold performed on every status refresh.
pub fn get_staged_line_stats(
	repo_path: &RepoPath,
	options: Option<DiffOptions>,
) -> Result<(usize, usize)> {
	scope_time!("get_staged_line_stats");

	let repo = repo(repo_path)?;
	let diff = staged_diff_raw(&repo, options)?;
	diff_line_stats(&diff)
}

/// total (added, deleted) lines across unstaged tracked files.
/// Untracked (new) files are excluded so the count reflects only
/// changes to files git already knows about.
///
/// Like [`get_staged_line_stats`], this runs a single index→workdir
/// diff (without `include_untracked`, so new files are naturally
/// excluded) and reads `git2::Diff::stats()`, rather than building a
/// full [`FileDiff`] per file.
pub fn get_unstaged_line_stats(
	repo_path: &RepoPath,
	options: Option<DiffOptions>,
) -> Result<(usize, usize)> {
	scope_time!("get_unstaged_line_stats");

	let repo = repo(repo_path)?;
	let diff = unstaged_diff_raw(&repo, options)?;
	diff_line_stats(&diff)
}

/// Build a single tree→index diff over the whole repo (no pathspec),
/// matching the `stage = true` branch of [`get_diff_raw`] but applied
/// to every staged file at once. No `find_similar`, matching the
/// per-file path's behavior.
fn staged_diff_raw(
	repo: &Repository,
	options: Option<DiffOptions>,
) -> Result<Diff<'_>> {
	let mut opt = diff_options(options);

	let diff = if let Ok(id) = get_head_repo(repo) {
		let parent = repo.find_commit(id.into())?;
		let tree = parent.tree()?;
		repo.diff_tree_to_index(
			Some(&tree),
			Some(&repo.index()?),
			Some(&mut opt),
		)?
	} else {
		repo.diff_tree_to_index(
			None,
			Some(&repo.index()?),
			Some(&mut opt),
		)?
	};

	Ok(diff)
}

/// Build a single index→workdir diff over the whole repo (no
/// pathspec, no `include_untracked`), matching the `stage = false`
/// branch of [`get_diff_raw`] minus the untracked-inclusion it sets.
/// Untracked files are intentionally excluded so the count reflects
/// only changes to tracked files.
fn unstaged_diff_raw(
	repo: &Repository,
	options: Option<DiffOptions>,
) -> Result<Diff<'_>> {
	let mut opt = diff_options(options);
	Ok(repo.diff_index_to_workdir(None, Some(&mut opt))?)
}

/// (insertions, deletions) for a whole-repo diff, via `git2`'s native
/// stats — no hunk/line parsing on our side.
fn diff_line_stats(diff: &Diff<'_>) -> Result<(usize, usize)> {
	let stats = diff.stats()?;
	Ok((stats.insertions(), stats.deletions()))
}

/// Build `git2::DiffOptions` from the user-facing [`DiffOptions`],
/// with no pathspec (whole-repo diff).
fn diff_options(options: Option<DiffOptions>) -> git2::DiffOptions {
	let mut opt = git2::DiffOptions::new();
	if let Some(options) = options {
		opt.context_lines(options.context);
		opt.ignore_whitespace(options.ignore_whitespace);
		opt.interhunk_lines(options.interhunk_lines);
	}
	opt
}

/// returns diff of a specific file inside a commit
/// see `get_commit_diff`
pub fn get_diff_commit(
	repo_path: &RepoPath,
	id: CommitId,
	p: String,
	options: Option<DiffOptions>,
) -> Result<FileDiff> {
	scope_time!("get_diff_commit");

	let repo = repo(repo_path)?;
	let work_dir = work_dir(&repo)?;
	let diff = get_commit_diff(
		&repo,
		id,
		Some(p),
		options,
		Some(&get_stashes(repo_path)?.into_iter().collect()),
	)?;

	raw_diff_to_file_diff(&diff, work_dir)
}

/// get file changes of a diff between two commits
pub fn get_diff_commits(
	repo_path: &RepoPath,
	ids: OldNew<CommitId>,
	p: String,
	options: Option<DiffOptions>,
) -> Result<FileDiff> {
	scope_time!("get_diff_commits");

	let repo = repo(repo_path)?;
	let work_dir = work_dir(&repo)?;
	let diff =
		get_compare_commits_diff(&repo, ids, Some(p), options)?;

	raw_diff_to_file_diff(&diff, work_dir)
}

///
//TODO: refactor into helper type with the inline closures as dedicated functions
#[allow(clippy::too_many_lines)]
fn raw_diff_to_file_diff(
	diff: &Diff,
	work_dir: &Path,
) -> Result<FileDiff> {
	let res = Rc::new(RefCell::new(FileDiff::default()));
	{
		let mut current_lines = Vec::new();
		let mut current_hunk: Option<HunkHeader> = None;

		let res_cell = Rc::clone(&res);
		let adder = move |header: &HunkHeader,
		                  lines: &Vec<DiffLine>| {
			let mut res = res_cell.borrow_mut();
			res.hunks.push(Hunk {
				header_hash: hash(header),
				lines: lines.clone(),
			});
			res.lines += lines.len();
		};

		let res_cell = Rc::clone(&res);
		let mut put = |delta: DiffDelta,
		               hunk: Option<DiffHunk>,
		               line: git2::DiffLine| {
			{
				let mut res = res_cell.borrow_mut();
				res.sizes = (
					delta.old_file().size(),
					delta.new_file().size(),
				);
				//TODO: use try_conv
				res.size_delta = (i64::conv(res.sizes.1))
					.saturating_sub(i64::conv(res.sizes.0));
			}
			if let Some(hunk) = hunk {
				let hunk_header = HunkHeader::from(hunk);

				match current_hunk {
					None => current_hunk = Some(hunk_header),
					Some(h) => {
						if h != hunk_header {
							adder(&h, &current_lines);
							current_lines.clear();
							current_hunk = Some(hunk_header);
						}
					}
				}

				let diff_line = DiffLine {
					position: DiffLinePosition::from(&line),
					content: String::from_utf8_lossy(line.content())
						//Note: trim await trailing newline characters
						.trim_matches(is_newline)
						.into(),
					line_type: line.origin_value().into(),
				};

				current_lines.push(diff_line);
			}
		};

		let new_file_diff = if diff.deltas().len() == 1 {
			if let Some(delta) = diff.deltas().next() {
				if delta.status() == Delta::Untracked {
					let relative_path =
						delta.new_file().path().ok_or_else(|| {
							Error::Generic(
								"new file path is unspecified."
									.to_string(),
							)
						})?;

					let newfile_path = work_dir.join(relative_path);

					if let Some(newfile_content) =
						new_file_content(&newfile_path)
					{
						let mut patch = Patch::from_buffers(
							&[],
							None,
							newfile_content.as_slice(),
							Some(&newfile_path),
							None,
						)?;

						patch.print(
							&mut |delta,
							      hunk: Option<DiffHunk>,
							      line: git2::DiffLine| {
								put(delta, hunk, line);
								true
							},
						)?;

						true
					} else {
						false
					}
				} else {
					false
				}
			} else {
				false
			}
		} else {
			false
		};

		if !new_file_diff {
			diff.print(
				DiffFormat::Patch,
				move |delta, hunk, line: git2::DiffLine| {
					put(delta, hunk, line);
					true
				},
			)?;
		}

		if !current_lines.is_empty() {
			adder(
				&current_hunk.map_or_else(
					|| Err(Error::Generic("invalid hunk".to_owned())),
					Ok,
				)?,
				&current_lines,
			);
		}

		if new_file_diff {
			res.borrow_mut().untracked = true;
		}
	}
	let res = Rc::try_unwrap(res)
		.map_err(|_| Error::Generic("rc unwrap error".to_owned()))?;
	Ok(res.into_inner())
}

const fn is_newline(c: char) -> bool {
	c == '\n' || c == '\r'
}

fn new_file_content(path: &Path) -> Option<Vec<u8>> {
	if let Ok(meta) = fs::symlink_metadata(path) {
		if meta.file_type().is_symlink() {
			if let Ok(path) = fs::read_link(path) {
				return Some(
					path.to_str()?.to_string().as_bytes().into(),
				);
			}
		} else if !meta.file_type().is_dir() {
			if let Ok(content) = fs::read(path) {
				return Some(content);
			}
		}
	}

	None
}

#[cfg(test)]
mod tests {
	use super::{
		get_diff, get_diff_commit, get_staged_line_stats,
		get_unstaged_line_stats,
	};
	use crate::{
		error::Result,
		sync::{
			commit, stage_add_file,
			status::{get_status, StatusType},
			tests::{get_statuses, repo_init, repo_init_empty},
			RepoPath,
		},
	};
	use std::{
		fs::{self, File},
		io::Write,
		path::Path,
	};

	#[test]
	fn test_untracked_subfolder() {
		let (_td, repo) = repo_init().unwrap();
		let root = repo.path().parent().unwrap();
		let repo_path: &RepoPath =
			&root.as_os_str().to_str().unwrap().into();

		assert_eq!(get_statuses(repo_path), (0, 0));

		fs::create_dir(root.join("foo")).unwrap();
		File::create(root.join("foo/bar.txt"))
			.unwrap()
			.write_all(b"test\nfoo")
			.unwrap();

		assert_eq!(get_statuses(repo_path), (1, 0));

		let diff =
			get_diff(repo_path, "foo/bar.txt", false, None).unwrap();

		assert_eq!(diff.hunks.len(), 1);
		assert_eq!(&*diff.hunks[0].lines[1].content, "test");
	}

	#[test]
	fn test_empty_repo() {
		let file_path = Path::new("foo.txt");
		let (_td, repo) = repo_init_empty().unwrap();
		let root = repo.path().parent().unwrap();
		let repo_path: &RepoPath =
			&root.as_os_str().to_str().unwrap().into();

		assert_eq!(get_statuses(repo_path), (0, 0));

		File::create(root.join(file_path))
			.unwrap()
			.write_all(b"test\nfoo")
			.unwrap();

		assert_eq!(get_statuses(repo_path), (1, 0));

		stage_add_file(repo_path, file_path).unwrap();

		assert_eq!(get_statuses(repo_path), (0, 1));

		let diff = get_diff(
			repo_path,
			file_path.to_str().unwrap(),
			true,
			None,
		)
		.unwrap();

		assert_eq!(diff.hunks.len(), 1);
	}

	#[test]
	fn test_staged_line_stats_empty() {
		let (_td, repo) = repo_init().unwrap();
		let root = repo.path().parent().unwrap();
		let repo_path: &RepoPath =
			&root.as_os_str().to_str().unwrap().into();

		assert_eq!(
			get_staged_line_stats(repo_path, None).unwrap(),
			(0, 0)
		);
	}

	#[test]
	fn test_staged_line_stats_add_delete() {
		let file_path = Path::new("foo.txt");
		let (_td, repo) = repo_init().unwrap();
		let root = repo.path().parent().unwrap();
		let repo_path: &RepoPath =
			&root.as_os_str().to_str().unwrap().into();

		// commit initial content
		File::create(root.join(file_path))
			.unwrap()
			.write_all(b"a\nb\nc\nd\n")
			.unwrap();
		stage_add_file(repo_path, file_path).unwrap();
		commit(repo_path, "init").unwrap();

		// stage a mix of additions and deletions
		fs::write(root.join(file_path), b"a\nB\nc\nd\ne\nf\n")
			.unwrap();
		stage_add_file(repo_path, file_path).unwrap();

		// +B, +e, +f (3 adds); original b line deleted (1 del)
		assert_eq!(
			get_staged_line_stats(repo_path, None).unwrap(),
			(3, 1)
		);
	}

	#[test]
	fn test_staged_line_stats_sums_across_files() {
		let (_td, repo) = repo_init().unwrap();
		let root = repo.path().parent().unwrap();
		let repo_path: &RepoPath =
			&root.as_os_str().to_str().unwrap().into();

		File::create(root.join("a.txt"))
			.unwrap()
			.write_all(b"first\n")
			.unwrap();
		stage_add_file(repo_path, Path::new("a.txt")).unwrap();
		commit(repo_path, "init").unwrap();

		File::create(root.join("a.txt"))
			.unwrap()
			.write_all(b"first\nsecond\n")
			.unwrap();
		stage_add_file(repo_path, Path::new("a.txt")).unwrap();

		File::create(root.join("b.txt"))
			.unwrap()
			.write_all(b"x\ny\n")
			.unwrap();
		stage_add_file(repo_path, Path::new("b.txt")).unwrap();

		// a.txt: +1 -0, b.txt (new file): +2 -0
		assert_eq!(
			get_staged_line_stats(repo_path, None).unwrap(),
			(3, 0)
		);
	}

	#[test]
	fn test_unstaged_line_stats_empty() {
		let (_td, repo) = repo_init().unwrap();
		let root = repo.path().parent().unwrap();
		let repo_path: &RepoPath =
			&root.as_os_str().to_str().unwrap().into();

		assert_eq!(
			get_unstaged_line_stats(repo_path, None).unwrap(),
			(0, 0)
		);
	}

	#[test]
	fn test_unstaged_line_stats_add_delete() {
		let file_path = Path::new("foo.txt");
		let (_td, repo) = repo_init().unwrap();
		let root = repo.path().parent().unwrap();
		let repo_path: &RepoPath =
			&root.as_os_str().to_str().unwrap().into();

		// commit initial content
		File::create(root.join(file_path))
			.unwrap()
			.write_all(b"a\nb\nc\nd\n")
			.unwrap();
		stage_add_file(repo_path, file_path).unwrap();
		commit(repo_path, "init").unwrap();

		// unstaged mix of additions and deletions
		fs::write(root.join(file_path), b"a\nB\nc\nd\ne\nf\n")
			.unwrap();

		// +B, +e, +f (3 adds); original b line deleted (1 del)
		assert_eq!(
			get_unstaged_line_stats(repo_path, None).unwrap(),
			(3, 1)
		);
	}

	#[test]
	fn test_unstaged_line_stats_sums_across_files() {
		let (_td, repo) = repo_init().unwrap();
		let root = repo.path().parent().unwrap();
		let repo_path: &RepoPath =
			&root.as_os_str().to_str().unwrap().into();

		File::create(root.join("a.txt"))
			.unwrap()
			.write_all(b"first\n")
			.unwrap();
		stage_add_file(repo_path, Path::new("a.txt")).unwrap();
		commit(repo_path, "init").unwrap();

		// a.txt: +1 -0 (unstaged)
		fs::write(root.join("a.txt"), b"first\nsecond\n").unwrap();

		// b.txt: +2 -0 (staged new file, should NOT count for unstaged)
		File::create(root.join("b.txt"))
			.unwrap()
			.write_all(b"x\ny\n")
			.unwrap();
		stage_add_file(repo_path, Path::new("b.txt")).unwrap();

		// only a.txt counts: +1 -0
		assert_eq!(
			get_unstaged_line_stats(repo_path, None).unwrap(),
			(1, 0)
		);
	}

	#[test]
	fn test_unstaged_line_stats_ignores_untracked() {
		let file_path = Path::new("foo.txt");
		let (_td, repo) = repo_init().unwrap();
		let root = repo.path().parent().unwrap();
		let repo_path: &RepoPath =
			&root.as_os_str().to_str().unwrap().into();

		// commit initial content
		File::create(root.join(file_path))
			.unwrap()
			.write_all(b"a\nb\n")
			.unwrap();
		stage_add_file(repo_path, file_path).unwrap();
		commit(repo_path, "init").unwrap();

		// tracked file: +1 -1
		fs::write(root.join(file_path), b"a\nB\nC\n").unwrap();

		// untracked new file with 5 added lines: should be excluded
		File::create(root.join("untracked.txt"))
			.unwrap()
			.write_all(b"1\n2\n3\n4\n5\n")
			.unwrap();

		// only the tracked file's +2 -1 counts
		assert_eq!(
			get_unstaged_line_stats(repo_path, None).unwrap(),
			(2, 1)
		);
	}

	/// Benchmark + correctness check: the single-bulk-diff
	/// `get_unstaged_line_stats` must produce the same counts as the
	/// old per-file fold over `get_diff`, and must be substantially
	/// faster. Re-implements the old algorithm inline (so we don't
	/// keep the slow path around in production code) and compares both
	/// against the same repo.
	///
	/// Marked `#[ignore]` because it is a timing benchmark (flaky on
	/// loaded/CI machines and not a correctness gate); run explicitly
	/// with `cargo test -p asyncgit bench_unstaged_line_stats -- --ignored`.
	#[test]
	#[ignore = "timing benchmark; run with --ignored"]
	fn bench_unstaged_line_stats() {
		use std::time::Instant;

		let (_td, repo) = repo_init().unwrap();
		let root = repo.path().parent().unwrap();
		let repo_path: &RepoPath =
			&root.as_os_str().to_str().unwrap().into();

		// Build a repo with many tracked files that each have unstaged
		// adds+deletes, so the per-file fold has real work to do.
		const NUM_FILES: usize = 80;
		const LINES_PER_FILE: usize = 60;

		for i in 0..NUM_FILES {
			let path = format!("file_{i:03}.txt");
			// initial committed content
			let initial: String = (0..LINES_PER_FILE)
				.map(|l| format!("line {l}\n"))
				.collect();
			fs::write(root.join(&path), initial.as_bytes()).unwrap();
			stage_add_file(repo_path, Path::new(&path)).unwrap();
		}
		commit(repo_path, "init").unwrap();

		// now mutate every tracked file: replace ~half the lines
		for i in 0..NUM_FILES {
			let path = format!("file_{i:03}.txt");
			let modified: String = (0..LINES_PER_FILE)
				.map(|l| {
					if l % 2 == 0 {
						format!("changed {l}\n")
					} else {
						format!("line {l}\n")
					}
				})
				.collect();
			fs::write(root.join(&path), modified.as_bytes()).unwrap();
		}

		// --- old per-file algorithm (re-implemented inline) ---
		let old_impl = |repo_path: &RepoPath,
		                options: Option<super::DiffOptions>|
		 -> (usize, usize) {
			use super::DiffLineType;
			let items = super::super::status::get_status(
				repo_path,
				super::super::status::StatusType::WorkingDir,
				None,
			)
			.unwrap();

			items.iter().fold(
				(0usize, 0usize),
				|(added, deleted), item| {
					if matches!(
						item.status,
						super::super::status::StatusItemType::New
					) {
						return (added, deleted);
					}

					let Ok(diff) = get_diff(
						repo_path, &item.path, false, options,
					) else {
						return (added, deleted);
					};

					diff.hunks
						.iter()
						.flat_map(|hunk| hunk.lines.iter())
						.fold((added, deleted), |(a, d), line| {
							match line.line_type {
								DiffLineType::Add => (a + 1, d),
								DiffLineType::Delete => (a, d + 1),
								_ => (a, d),
							}
						})
				},
			)
		};

		// Warm up (populate any git2/index caches) so we compare the
		// steady-state cost, not first-touch disk reads.
		let _ = old_impl(repo_path, None);
		let _ = get_unstaged_line_stats(repo_path, None).unwrap();

		// Correctness: both must agree.
		let expected = old_impl(repo_path, None);
		let actual =
			get_unstaged_line_stats(repo_path, None).unwrap();
		assert_eq!(
			actual, expected,
			"optimized line stats diverged from per-file fold"
		);
		// sanity: the benchmark actually has non-trivial changes
		assert!(
			expected.0 > 0 && expected.1 > 0,
			"benchmark repo produced no changes: {expected:?}"
		);

		// Timing: run each a few times and take the median to dampen
		// scheduling noise.
		const RUNS: usize = 5;
		let time_old = {
			let mut samples: Vec<u128> = (0..RUNS)
				.map(|_| {
					let t = Instant::now();
					let _ = old_impl(repo_path, None);
					t.elapsed().as_micros()
				})
				.collect();
			samples.sort_unstable();
			samples[samples.len() / 2]
		};
		let time_new = {
			let mut samples: Vec<u128> = (0..RUNS)
				.map(|_| {
					let t = Instant::now();
					let _ = get_unstaged_line_stats(repo_path, None)
						.unwrap();
					t.elapsed().as_micros()
				})
				.collect();
			samples.sort_unstable();
			samples[samples.len() / 2]
		};

		eprintln!(
			"bench_unstaged_line_stats: {NUM_FILES} files, \
			 {expected:?} — old={time_old}us, new={time_new}us, \
			 speedup={:.1}x",
			time_old as f64 / time_new.max(1) as f64
		);

		// The bulk-stats path should be clearly faster. Use a generous
		// threshold (2x) so this stays green on slow/loaded machines
		// while still catching a regression that removes the win.
		assert!(
			time_new * 2 < time_old,
			"optimized path not faster: old={time_old}us new={time_new}us"
		);
	}

	static HUNK_A: &str = r"
1   start
2
3
4
5
6   middle
7
8
9
0
1   end";

	static HUNK_B: &str = r"
1   start
2   newa
3
4
5
6   middle
7
8
9
0   newb
1   end";

	#[test]
	fn test_hunks() {
		let (_td, repo) = repo_init().unwrap();
		let root = repo.path().parent().unwrap();
		let repo_path: &RepoPath =
			&root.as_os_str().to_str().unwrap().into();

		assert_eq!(get_statuses(repo_path), (0, 0));

		let file_path = root.join("bar.txt");

		{
			File::create(&file_path)
				.unwrap()
				.write_all(HUNK_A.as_bytes())
				.unwrap();
		}

		let res = get_status(repo_path, StatusType::WorkingDir, None)
			.unwrap();
		assert_eq!(res.len(), 1);
		assert_eq!(res[0].path, "bar.txt");

		stage_add_file(repo_path, Path::new("bar.txt")).unwrap();
		assert_eq!(get_statuses(repo_path), (0, 1));

		// overwrite with next content
		{
			File::create(&file_path)
				.unwrap()
				.write_all(HUNK_B.as_bytes())
				.unwrap();
		}

		assert_eq!(get_statuses(repo_path), (1, 1));

		let res =
			get_diff(repo_path, "bar.txt", false, None).unwrap();

		assert_eq!(res.hunks.len(), 2);
	}

	#[test]
	fn test_diff_newfile_in_sub_dir_current_dir() {
		let file_path = Path::new("foo/foo.txt");
		let (_td, repo) = repo_init_empty().unwrap();
		let root = repo.path().parent().unwrap();

		let sub_path = root.join("foo/");

		fs::create_dir_all(&sub_path).unwrap();
		File::create(root.join(file_path))
			.unwrap()
			.write_all(b"test")
			.unwrap();

		let diff = get_diff(
			&sub_path.to_str().unwrap().into(),
			file_path.to_str().unwrap(),
			false,
			None,
		)
		.unwrap();

		assert_eq!(&*diff.hunks[0].lines[1].content, "test");
	}

	#[test]
	fn test_diff_delta_size() -> Result<()> {
		let file_path = Path::new("bar");
		let (_td, repo) = repo_init_empty().unwrap();
		let root = repo.path().parent().unwrap();
		let repo_path: &RepoPath =
			&root.as_os_str().to_str().unwrap().into();

		File::create(root.join(file_path))?.write_all(b"\x00")?;

		stage_add_file(repo_path, file_path).unwrap();

		commit(repo_path, "commit").unwrap();

		File::create(root.join(file_path))?.write_all(b"\x00\x02")?;

		let diff = get_diff(
			repo_path,
			file_path.to_str().unwrap(),
			false,
			None,
		)
		.unwrap();

		dbg!(&diff);
		assert_eq!(diff.sizes, (1, 2));
		assert_eq!(diff.size_delta, 1);

		Ok(())
	}

	#[test]
	fn test_binary_diff_delta_size_untracked() -> Result<()> {
		let file_path = Path::new("bar");
		let (_td, repo) = repo_init_empty().unwrap();
		let root = repo.path().parent().unwrap();
		let repo_path: &RepoPath =
			&root.as_os_str().to_str().unwrap().into();

		File::create(root.join(file_path))?.write_all(b"\x00\xc7")?;

		let diff = get_diff(
			repo_path,
			file_path.to_str().unwrap(),
			false,
			None,
		)
		.unwrap();

		dbg!(&diff);
		assert_eq!(diff.sizes, (0, 2));
		assert_eq!(diff.size_delta, 2);

		Ok(())
	}

	#[test]
	fn test_diff_delta_size_commit() -> Result<()> {
		let file_path = Path::new("bar");
		let (_td, repo) = repo_init_empty().unwrap();
		let root = repo.path().parent().unwrap();
		let repo_path: &RepoPath =
			&root.as_os_str().to_str().unwrap().into();

		File::create(root.join(file_path))?.write_all(b"\x00")?;

		stage_add_file(repo_path, file_path).unwrap();

		commit(repo_path, "").unwrap();

		File::create(root.join(file_path))?.write_all(b"\x00\x02")?;

		stage_add_file(repo_path, file_path).unwrap();

		let id = commit(repo_path, "").unwrap();

		let diff =
			get_diff_commit(repo_path, id, String::new(), None)
				.unwrap();

		dbg!(&diff);
		assert_eq!(diff.sizes, (1, 2));
		assert_eq!(diff.size_delta, 1);

		Ok(())
	}
}
