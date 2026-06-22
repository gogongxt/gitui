use std::{
	borrow::Cow,
	collections::{HashMap, HashSet, VecDeque},
	hash::{Hash, Hasher},
	path::{Path, PathBuf},
	sync::{
		atomic::{AtomicU64, Ordering},
		Arc, Mutex,
	},
};

use asyncgit::sync::{diff::DiffLinePosition, RepoPath};
use asyncgit::AsyncGitNotification;
use asyncgit::{DiffLineType, FileDiff};
use crossbeam_channel::Sender;
use ratatui::{
	style::{Color, Style},
	text::{Line, Span},
};

use crate::ansi::ansi_to_lines;

/// Parameters identifying a delta render request.
///
/// `diff_hash` is the hash of the `FileDiff` (from `asyncgit::hash`).
/// Including it ensures the cache invalidates when the file's diff
/// content changes (stage/unstage, external edits) even though
/// `path`/`diff_type`/`width` stay the same.
#[derive(Clone, PartialEq, Eq)]
pub struct DeltaParams {
	pub path: String,
	pub diff_type: asyncgit::DiffType,
	pub width: u16,
	pub side_by_side: bool,
	pub diff_hash: u64,
}

impl Hash for DeltaParams {
	fn hash<H: Hasher>(&self, state: &mut H) {
		self.path.hash(state);
		self.diff_type.hash(state);
		self.width.hash(state);
		self.side_by_side.hash(state);
		self.diff_hash.hash(state);
	}
}

/// Fully-processed delta output ready to be rendered.
/// Built on the worker thread so the main thread only does assignment.
#[derive(Clone)]
pub struct ProcessedDelta {
	pub display_lines: Vec<Line<'static>>,
	pub display_hunks: Vec<usize>,
	pub display_positions: Vec<Option<DiffLinePosition>>,
	pub line_level_bgs: Vec<Option<Color>>,
}

/// Hashed key for cache entries.
type CacheKey = u64;

struct Entry {
	key: CacheKey,
	result: ProcessedDelta,
}

/// LRU cache for rendered delta output. Capacity 50.
/// On hit, the entry is moved to the back (most-recent).
/// On miss+insert when full, the front (least-recent) is evicted.
struct LruCache {
	entries: VecDeque<Entry>,
	capacity: usize,
}

impl LruCache {
	fn new(capacity: usize) -> Self {
		Self {
			entries: VecDeque::with_capacity(capacity),
			capacity,
		}
	}

	fn get(&mut self, key: CacheKey) -> Option<&ProcessedDelta> {
		let pos = self.entries.iter().position(|e| e.key == key)?;
		let entry = self.entries.remove(pos)?;
		self.entries.push_back(entry);
		self.entries.back().map(|e| &e.result)
	}

	fn insert(&mut self, key: CacheKey, result: ProcessedDelta) {
		if let Some(pos) =
			self.entries.iter().position(|e| e.key == key)
		{
			self.entries.remove(pos);
		}
		if self.entries.len() >= self.capacity {
			self.entries.pop_front();
		}
		self.entries.push_back(Entry { key, result });
	}
}

/// Async worker that runs `git diff | delta` on a rayon thread.
/// Results are cached by (path, `diff_type`, width, `side_by_side`).
pub struct AsyncDelta {
	sender: Sender<AsyncGitNotification>,
	cache: Arc<Mutex<LruCache>>,
	/// Keys of in-flight requests, to dedupe rapid re-requests.
	in_flight: Arc<Mutex<HashSet<CacheKey>>>,
	/// The most recently completed result, with its key.
	/// `take_if_matches` consumes this if the key matches.
	last_completed: Arc<Mutex<Option<(CacheKey, ProcessedDelta)>>>,
	pending_count: Arc<AtomicU64>,
}

impl AsyncDelta {
	pub fn new(sender: &Sender<AsyncGitNotification>) -> Self {
		Self {
			sender: sender.clone(),
			cache: Arc::new(Mutex::new(LruCache::new(50))),
			in_flight: Arc::new(Mutex::new(HashSet::new())),
			last_completed: Arc::new(Mutex::new(None)),
			pending_count: Arc::new(AtomicU64::new(0)),
		}
	}

	/// Request a delta render. Returns `Some(result)` on cache hit,
	/// `None` on cache miss (a background job is spawned; when it
	/// finishes, `AsyncGitNotification::Delta` is sent and
	/// `take_if_matches()` will return the result).
	///
	/// If a request with the same key is already in-flight, returns
	/// `None` without spawning a duplicate — the existing job will
	/// notify when done.
	///
	/// `diff` is the `FileDiff` from asyncgit, used to build hunk
	/// and line-number mappings on the worker thread.
	pub fn request(
		&self,
		params: DeltaParams,
		repo: RepoPath,
		diff: Option<FileDiff>,
	) -> Option<ProcessedDelta> {
		let key = hash_params(&params);

		// Cache hit?
		{
			let mut cache = self.cache.lock().ok()?;
			if let Some(result) = cache.get(key) {
				return Some(result.clone());
			}
		}

		// Dedupe: if a request with this key is already in-flight,
		// don't spawn another. The existing job will notify on done.
		{
			let mut in_flight = self.in_flight.lock().ok()?;
			if !in_flight.insert(key) {
				return None;
			}
		}

		let cache = Arc::clone(&self.cache);
		let in_flight = Arc::clone(&self.in_flight);
		let last_completed = Arc::clone(&self.last_completed);
		let sender = self.sender.clone();
		let pending_count = Arc::clone(&self.pending_count);

		pending_count.fetch_add(1, Ordering::Relaxed);
		rayon_core::spawn(move || {
			let result = run_delta(&repo, &params, diff.as_ref());

			// Remove from in-flight first, then publish the result.
			// Order matters: a re-request after removal will spawn
			// a fresh job (cache miss), which is fine.
			if let Ok(mut set) = in_flight.lock() {
				set.remove(&key);
			}
			if let Some(processed) = result {
				if let Ok(mut cache) = cache.lock() {
					cache.insert(key, processed.clone());
				}
				if let Ok(mut last) = last_completed.lock() {
					*last = Some((key, processed));
				}
			}
			pending_count.fetch_sub(1, Ordering::Relaxed);
			let _ = sender.send(AsyncGitNotification::Delta);
		});

		None
	}

	/// Returns true if a request is in-flight.
	pub fn is_pending(&self) -> bool {
		self.pending_count.load(Ordering::Relaxed) > 0
	}

	/// Consume the most recently completed result if its key matches
	/// `expected`. Returns `None` on mismatch (stale) or if nothing
	/// has completed since the last call.
	pub fn take_if_matches(
		&self,
		expected: &DeltaParams,
	) -> Option<ProcessedDelta> {
		let expected_key = hash_params(expected);
		let mut last = self.last_completed.lock().ok()?;
		match last.take() {
			Some((key, result)) if key == expected_key => {
				Some(result)
			}
			other => {
				*last = other;
				None
			}
		}
	}
}

fn hash_params(params: &DeltaParams) -> CacheKey {
	let mut hasher = std::collections::hash_map::DefaultHasher::new();
	params.hash(&mut hasher);
	hasher.finish()
}

/// Run `git diff | delta`, parse output, and build display lines.
/// `None` on failure or empty diff.
#[allow(clippy::too_many_lines)]
fn run_delta(
	repo: &RepoPath,
	params: &DeltaParams,
	diff: Option<&FileDiff>,
) -> Option<ProcessedDelta> {
	let path = &params.path;
	if path.is_empty() {
		return None;
	}

	// Determine workdir: prefer RepoPath's workdir, fall back to
	// `git rev-parse --show-toplevel`.
	let work_dir =
		repo.workdir().map(Path::to_path_buf).or_else(|| {
			let output = std::process::Command::new("git")
				.args(["rev-parse", "--show-toplevel"])
				.current_dir(repo.gitpath())
				.output()
				.ok()?;
			if output.status.success() {
				let p = String::from_utf8_lossy(&output.stdout);
				Some(PathBuf::from(p.trim()))
			} else {
				None
			}
		});

	let work_dir = work_dir?;

	let mut git_cmd = std::process::Command::new("git");
	match &params.diff_type {
		asyncgit::DiffType::WorkDir => {
			git_cmd.arg("diff");
			git_cmd.arg(path);
		}
		asyncgit::DiffType::Stage => {
			git_cmd.arg("diff");
			git_cmd.arg("--cached");
			git_cmd.arg(path);
		}
		asyncgit::DiffType::Commit(commit_id) => {
			git_cmd.arg("diff");
			let range = format!("{commit_id}^..{commit_id}");
			git_cmd.arg(&range);
			git_cmd.arg("--");
			git_cmd.arg(path);
		}
		asyncgit::DiffType::Commits(ids) => {
			git_cmd.arg("diff");
			git_cmd.arg(ids.old.to_string());
			git_cmd.arg(ids.new.to_string());
			git_cmd.arg("--");
			git_cmd.arg(path);
		}
	}

	let mut git_output =
		git_cmd.current_dir(&work_dir).output().ok()?;
	if !git_output.status.success() {
		if let asyncgit::DiffType::Commit(commit_id) =
			&params.diff_type
		{
			let empty_tree =
				"4b825dc642cb6eb9a060e54bf899d15f3f9381b1";
			let fallback = std::process::Command::new("git")
				.args([
					"diff",
					empty_tree,
					&commit_id.to_string(),
					"--",
					path,
				])
				.current_dir(&work_dir)
				.output();
			if let Ok(output) = fallback {
				if output.status.success() {
					git_output = output;
				} else {
					return None;
				}
			} else {
				return None;
			}
		} else {
			return None;
		}
	}

	if git_output.stdout.is_empty() {
		log::debug!(
			"delta: git diff produced empty output for {:?} {path}",
			params.diff_type
		);
	}

	// For untracked files, git diff produces empty output.
	// Fall back to diffing against /dev/null to show the full file as added.
	let git_output = if git_output.stdout.is_empty()
		&& matches!(params.diff_type, asyncgit::DiffType::WorkDir)
	{
		let fallback = std::process::Command::new("git")
			.args(["diff", "--no-index", "/dev/null", path])
			.current_dir(&work_dir)
			.output()
			.ok()?;
		if fallback.status.success()
			|| fallback.status.code() == Some(1)
		{
			fallback
		} else {
			git_output
		}
	} else {
		git_output
	};

	if git_output.stdout.is_empty() {
		log::debug!(
			"delta: no diff content for {:?} {path}",
			params.diff_type
		);
		return None;
	}

	let mut delta_args = vec![
		"--file-style".to_string(),
		"omit".to_string(),
		"--line-numbers".to_string(),
	];
	if params.side_by_side {
		delta_args.push("--width".to_string());
		delta_args.push(params.width.max(20).to_string());
		delta_args.push("--side-by-side".to_string());
	}

	let delta_bytes = if let Ok(mut child) =
		std::process::Command::new("delta")
			.args(&delta_args)
			.current_dir(&work_dir)
			.stdin(std::process::Stdio::piped())
			.stdout(std::process::Stdio::piped())
			.stderr(std::process::Stdio::null())
			.spawn()
	{
		let stdin_data = git_output.stdout;
		let mut child_stdin = child.stdin.take();
		let stdin_thread = std::thread::spawn(move || {
			if let Some(ref mut stdin) = child_stdin {
				use std::io::Write;
				let _ = stdin.write_all(&stdin_data);
			}
		});
		let output = child.wait_with_output().ok();
		let _ = stdin_thread.join();
		output.filter(|o| o.status.success()).map(|o| o.stdout)
	} else {
		log::error!("delta: failed to spawn delta process");
		None
	};

	if delta_bytes.is_none() {
		log::error!(
			"delta: delta process failed or produced no output for {:?} {path}",
			params.diff_type
		);
	}

	let (raw_lines, line_level_bgs) = delta_bytes.map_or_else(
		|| (Vec::new(), Vec::new()),
		|bytes| {
			let text = String::from_utf8_lossy(&bytes);
			ansi_to_lines(&text)
		},
	);

	Some(rebuild(
		raw_lines,
		line_level_bgs,
		diff,
		usize::from(params.width),
		params.side_by_side,
	))
}

/// Build display lines, hunk map, and position map from raw delta
/// output. Runs on the worker thread.
#[allow(clippy::too_many_lines)]
fn rebuild(
	raw_lines: Vec<Line<'static>>,
	line_level_bgs: Vec<Option<Color>>,
	diff: Option<&FileDiff>,
	panel_width: usize,
	side_by_side: bool,
) -> ProcessedDelta {
	// Build old/new lineno lookups from FileDiff for position mapping
	let (old_lineno_lookup, new_lineno_lookup): (
		HashMap<u32, DiffLinePosition>,
		HashMap<u32, DiffLinePosition>,
	) = diff.map_or_else(
		|| (HashMap::new(), HashMap::new()),
		|diff| {
			let mut old_map = HashMap::new();
			let mut new_map = HashMap::new();
			for hunk in &diff.hunks {
				for line in &hunk.lines {
					match line.line_type {
						DiffLineType::Add => {
							if let Some(n) = line.position.new_lineno
							{
								new_map.insert(n, line.position);
							}
						}
						DiffLineType::Delete => {
							if let Some(n) = line.position.old_lineno
							{
								old_map.insert(n, line.position);
							}
						}
						_ => {
							if let Some(n) = line.position.old_lineno
							{
								old_map
									.entry(n)
									.or_insert(line.position);
							}
							if let Some(n) = line.position.new_lineno
							{
								new_map
									.entry(n)
									.or_insert(line.position);
							}
						}
					}
				}
			}
			(old_map, new_map)
		},
	);

	// Build per-raw-line hunk map and position map
	let mut hunk_map: Vec<usize> =
		Vec::with_capacity(raw_lines.len());
	let mut pos_map: Vec<Option<DiffLinePosition>> =
		Vec::with_capacity(raw_lines.len());
	{
		let mut current_hunk: usize = 0;
		let mut first_separator_seen = false;
		for line in &raw_lines {
			let text: String = line
				.spans
				.iter()
				.map(|s| s.content.as_ref())
				.collect();
			let trimmed = text.trim();

			let is_top_separator = !trimmed.is_empty()
				&& trimmed.ends_with('\u{2510}')
				&& trimmed.chars().all(|c| {
					c == '\u{2500}'
						|| c == '\u{2510}' || c.is_whitespace()
				});
			if is_top_separator {
				if first_separator_seen {
					current_hunk += 1;
				}
				first_separator_seen = true;
			}
			hunk_map.push(current_hunk);

			let (old_lineno, new_lineno) =
				parse_delta_line_numbers(line);
			let pos = new_lineno.map_or_else(
				|| {
					old_lineno.and_then(|old_n| {
						old_lineno_lookup.get(&old_n).copied()
					})
				},
				|new_n| new_lineno_lookup.get(&new_n).copied(),
			);
			pos_map.push(pos);
		}
	}

	let pos_count = pos_map.iter().filter(|p| p.is_some()).count();
	log::debug!(
		"rebuild: {} raw lines, {} hunks, {} positions, width={panel_width}, sbs={side_by_side}",
		raw_lines.len(),
		hunk_map.iter().max().map_or(0, |h| h + 1),
		pos_count,
	);

	// Side-by-side: display lines = raw lines (no wrapping)
	if side_by_side || panel_width == 0 {
		return ProcessedDelta {
			display_lines: raw_lines,
			display_hunks: hunk_map,
			display_positions: pos_map,
			line_level_bgs,
		};
	}

	// Non-SBS: wrap each raw line to panel_width and pad bg
	let mut display_lines = Vec::new();
	let mut display_hunks = Vec::new();
	let mut display_positions = Vec::new();

	for (idx, line) in raw_lines.iter().enumerate() {
		// ansi_to_lines adds a trailing space for \x1b[K (erase-in-line)
		// to make bg visible. When the line already fills the panel width,
		// this extra space causes a spurious wrap. Strip it only in that case.
		let content_len: usize = line
			.spans
			.iter()
			.map(|s| s.content.chars().count())
			.sum();
		let trimmed_line = if content_len > panel_width {
			let mut spans: Vec<Span<'static>> = line.spans.clone();
			while spans
				.last()
				.is_some_and(|s| s.content.trim().is_empty())
			{
				spans.pop();
			}
			Line::from(spans)
		} else {
			line.clone()
		};
		let dominant_bg = line_level_bgs.get(idx).copied().flatten();
		let wrapped = wrap_line(&trimmed_line, panel_width);
		for wline in wrapped {
			let padded =
				pad_line_bg(wline, panel_width, false, dominant_bg);
			display_hunks
				.push(hunk_map.get(idx).copied().unwrap_or(0));
			display_positions
				.push(pos_map.get(idx).copied().unwrap_or(None));
			display_lines.push(padded);
		}
	}

	ProcessedDelta {
		display_lines,
		display_hunks,
		display_positions,
		line_level_bgs,
	}
}

/// Parse old and new line numbers from delta gutter spans.
/// Delta non-side-by-side format: `old_lineno ⋮ new_lineno │ content`
fn parse_delta_line_numbers(
	line: &Line<'_>,
) -> (Option<u32>, Option<u32>) {
	let spans = &line.spans;
	if spans.len() < 4 {
		return (None, None);
	}
	let old_num_text = spans[0].content.trim();
	let sep = spans[1].content.trim();
	if sep != "\u{22ee}" {
		return (None, None);
	}
	let new_num_text = spans[2].content.trim();

	let old_lineno = if old_num_text.is_empty()
		|| !old_num_text.chars().all(|c| c.is_ascii_digit())
	{
		None
	} else {
		old_num_text.parse().ok()
	};
	let new_lineno = if new_num_text.is_empty()
		|| !new_num_text.chars().all(|c| c.is_ascii_digit())
	{
		None
	} else {
		new_num_text.parse().ok()
	};

	(old_lineno, new_lineno)
}

pub fn wrap_line(
	line: &Line<'static>,
	width: usize,
) -> Vec<Line<'static>> {
	if width == 0 {
		return vec![line.clone()];
	}

	let mut result = Vec::new();
	let mut current_spans: Vec<Span<'static>> = Vec::new();
	let mut current_width = 0;

	for span in &line.spans {
		let mut remaining = span.content.as_ref();
		let style = span.style;

		loop {
			if remaining.is_empty() {
				break;
			}
			let space = width.saturating_sub(current_width);
			if space == 0 {
				result.push(Line::from(std::mem::take(
					&mut current_spans,
				)));
				current_width = 0;
				continue;
			}

			let char_count = remaining.chars().count();
			if char_count <= space {
				current_spans.push(Span::styled(
					Cow::Owned(remaining.to_string()),
					style,
				));
				current_width += char_count;
				break;
			}

			let mut byte_end = remaining.len();
			for (idx, (i, _)) in remaining.char_indices().enumerate()
			{
				if idx == space {
					byte_end = i;
					break;
				}
			}
			current_spans.push(Span::styled(
				Cow::Owned(remaining[..byte_end].to_string()),
				style,
			));
			remaining = &remaining[byte_end..];
			result
				.push(Line::from(std::mem::take(&mut current_spans)));
			current_width = 0;
		}
	}

	if !current_spans.is_empty() {
		result.push(Line::from(current_spans));
	}

	if result.is_empty() {
		result.push(Line::from(""));
	}

	result
}

/// Pad a line's last span with spaces so the bg extends to full width.
#[allow(clippy::too_many_lines)]
pub fn pad_line_bg(
	mut line: Line<'static>,
	width: usize,
	is_sbs: bool,
	dominant_bg: Option<Color>,
) -> Line<'static> {
	let content_width: usize =
		line.spans.iter().map(|s| s.content.chars().count()).sum();
	let bg_idx =
		line.spans.iter().rposition(|s| s.style.bg.is_some());
	let Some(idx) = bg_idx else {
		return line;
	};
	let bg_style = if let Some(dom) = dominant_bg {
		if let Some(s) = line.spans[..=idx]
			.iter()
			.find(|s| s.style.bg == Some(dom))
		{
			s.style
		} else {
			let fg = line.spans[..=idx]
				.iter()
				.rev()
				.find(|s| {
					s.style.bg.is_some() && s.style.fg.is_some()
				})
				.and_then(|s| s.style.fg);
			let mut s = Style::default().bg(dom);
			if let Some(fg) = fg {
				s = s.fg(fg);
			}
			s
		}
	} else {
		line.spans[idx].style
	};

	let inherited_fg = line.spans[..=idx]
		.iter()
		.rev()
		.find(|s| s.style.bg.is_some() && s.style.fg.is_some())
		.and_then(|s| s.style.fg);

	let boundary = if is_sbs {
		line.spans[idx + 1..]
			.iter()
			.position(|s| {
				s.style.bg.is_none()
					&& (matches!(s.style.fg, Some(Color::Indexed(_)))
						|| s.style.fg == Some(Color::Blue)
						|| s.content.chars().any(|c| c == '\u{2502}'))
			})
			.map(|p| idx + 1 + p)
	} else {
		None
	};

	let apply_end = boundary.unwrap_or(line.spans.len());
	for span in &mut line.spans[idx + 1..apply_end] {
		span.style =
			span.style.bg(bg_style.bg.expect("checked above"));
		if let Some(fg) = inherited_fg {
			span.style = span.style.fg(fg);
		}
	}
	let pad_limit = if is_sbs {
		boundary.map_or(content_width, |b| {
			line.spans[..b]
				.iter()
				.map(|s| s.content.chars().count())
				.sum()
		})
	} else {
		width
	};
	let left_panel_width: usize = line.spans[..apply_end]
		.iter()
		.map(|s| s.content.chars().count())
		.sum();
	if left_panel_width < pad_limit {
		let pad = pad_limit - left_panel_width;
		line.spans.insert(
			apply_end,
			Span::styled(Cow::Owned(" ".repeat(pad)), bg_style),
		);
	}
	line
}
