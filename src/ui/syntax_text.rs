use asyncgit::{
	asyncjob::{AsyncJob, RunParams},
	ProgressPercent,
};
use once_cell::sync::{Lazy, OnceCell};
use ratatui::text::{Line, Span};
use scopetime::scope_time;
use std::{
	io::Write,
	ops::Range,
	path::{Path, PathBuf},
	process::{Command, Stdio},
	sync::{Arc, Mutex},
	thread,
	time::{Duration, Instant},
};
use syntect::{
	highlighting::{
		FontStyle, HighlightState, Highlighter,
		RangedHighlightIterator, Style, Theme, ThemeSet,
	},
	parsing::{ParseState, ScopeStack, SyntaxSet},
};

use crate::{AsyncAppNotification, SyntaxHighlightProgress};

pub const DEFAULT_SYNTAX_THEME: &str = "base16-eighties.dark";

struct SyntaxLine {
	items: Vec<(Style, usize, Range<usize>)>,
}

/// Highlighted text from either `bat` (ANSI, parsed into ratatui `Line`s)
/// or the built-in `syntect` pipeline. `From<&SyntaxText> for Text` handles
/// both variants so callers stay source-agnostic.
pub struct SyntaxText {
	inner: SyntaxTextInner,
	path: PathBuf,
}

enum SyntaxTextInner {
	Syntect {
		text: String,
		lines: Vec<SyntaxLine>,
	},
	Ansi(Vec<Line<'static>>),
}

static SYNTAX_SET: Lazy<SyntaxSet> =
	Lazy::new(two_face::syntax::extra_no_newlines);
static THEME: OnceCell<Theme> = OnceCell::new();

pub struct AsyncProgressBuffer {
	current: usize,
	total: usize,
	last_send: Option<Instant>,
	min_interval: Duration,
}

impl AsyncProgressBuffer {
	pub const fn new(total: usize, min_interval: Duration) -> Self {
		Self {
			current: 0,
			total,
			last_send: None,
			min_interval,
		}
	}

	pub fn send_progress(&mut self) -> ProgressPercent {
		self.last_send = Some(Instant::now());
		ProgressPercent::new(self.current, self.total)
	}

	pub fn update(&mut self, current: usize) -> bool {
		self.current = current;
		self.last_send.is_none_or(|last_send| {
			last_send.elapsed() > self.min_interval
		})
	}
}

impl SyntaxText {
	pub fn new(
		text: String,
		file_path: &Path,
		params: &RunParams<AsyncAppNotification, ProgressPercent>,
		syntax: &str,
	) -> asyncgit::Result<Self> {
		scope_time!("syntax_highlighting");
		let mut state = {
			scope_time!("syntax_highlighting.0");
			let plain_text = || SYNTAX_SET.find_syntax_plain_text();
			let syntax = SYNTAX_SET
				.find_syntax_for_file(file_path)
				.unwrap_or_else(|e| {
					log::error!("Could not read the file to detect its syntax: {e}");
					Some(plain_text())
				})
				.unwrap_or_else(plain_text);

			ParseState::new(syntax)
		};

		let theme = THEME.get_or_try_init(|| -> Result<Theme, asyncgit::Error> {
			let theme_path = crate::args::get_app_config_path()
				.map_err(|e| asyncgit::Error::Generic(e.to_string()))?.join(format!("{syntax}.tmTheme"));

			match ThemeSet::get_theme(&theme_path) {
				Ok(t) => return Ok(t),
			    Err(e) => log::info!("could not load '{}': {e}, trying from the set of default themes", theme_path.display()),
			}

			let mut theme_set = ThemeSet::load_defaults();
			if let Some(t) = theme_set.themes.remove(syntax) {
			    return Ok(t);
			}

			log::error!("the syntax theme '{syntax}' cannot be found. Using default theme ('{DEFAULT_SYNTAX_THEME}') instead");
			Ok(theme_set.themes.remove(DEFAULT_SYNTAX_THEME).expect("the default theme should be there"))
		})?;

		let highlighter = Highlighter::new(theme);
		let mut syntax_lines: Vec<SyntaxLine> = Vec::new();

		let mut highlight_state =
			HighlightState::new(&highlighter, ScopeStack::new());

		{
			let total_count = text.lines().count();

			let mut buffer = AsyncProgressBuffer::new(
				total_count,
				Duration::from_millis(200),
			);
			params.set_progress(buffer.send_progress())?;
			params.send(AsyncAppNotification::SyntaxHighlighting(
				SyntaxHighlightProgress::Progress,
			))?;

			for (number, line) in text.lines().enumerate() {
				let ops = state
					.parse_line(line, &SYNTAX_SET)
					.map_err(|e| {
						log::error!("syntax error: {e:?}");
						asyncgit::Error::Generic(
							"syntax error".to_string(),
						)
					})?;
				let iter = RangedHighlightIterator::new(
					&mut highlight_state,
					&ops[..],
					line,
					&highlighter,
				);

				syntax_lines.push(SyntaxLine {
					items: iter
						.map(|(style, _, range)| {
							(style, number, range)
						})
						.collect(),
				});

				if buffer.update(number) {
					params.set_progress(buffer.send_progress())?;
					params.send(
						AsyncAppNotification::SyntaxHighlighting(
							SyntaxHighlightProgress::Progress,
						),
					)?;
				}
			}
		}

		Ok(Self {
			inner: SyntaxTextInner::Syntect {
				text,
				lines: syntax_lines,
			},
			path: file_path.into(),
		})
	}

	/// Build a `SyntaxText` from ANSI-colored output (e.g. from `bat`).
	/// `path` is used as the identity for cache-hit checks in consumers
	/// and as `--file-name` for `bat` syntax detection upstream.
	pub const fn from_ansi(
		lines: Vec<Line<'static>>,
		path: PathBuf,
	) -> Self {
		Self {
			inner: SyntaxTextInner::Ansi(lines),
			path,
		}
	}

	///
	pub fn path(&self) -> &Path {
		&self.path
	}
}

impl<'a> From<&'a SyntaxText> for ratatui::text::Text<'a> {
	fn from(v: &'a SyntaxText) -> Self {
		match &v.inner {
			SyntaxTextInner::Ansi(lines) => {
				ratatui::text::Text::from(lines.clone())
			}
			SyntaxTextInner::Syntect { text, lines } => {
				let mut result_lines: Vec<Line> =
					Vec::with_capacity(lines.len());

				for (syntax_line, line_content) in
					lines.iter().zip(text.lines())
				{
					let mut line_span: Line =
						Vec::with_capacity(syntax_line.items.len())
							.into();

					for (style, _, range) in &syntax_line.items {
						let item_content =
							&line_content[range.clone()];
						let item_style = syntact_style_to_tui(style);

						line_span.spans.push(Span::styled(
							item_content,
							item_style,
						));
					}

					result_lines.push(line_span);
				}

				result_lines.into()
			}
		}
	}
}

fn syntact_style_to_tui(style: &Style) -> ratatui::style::Style {
	let mut res = ratatui::style::Style::default().fg(
		ratatui::style::Color::Rgb(
			style.foreground.r,
			style.foreground.g,
			style.foreground.b,
		),
	);

	if style.font_style.contains(FontStyle::BOLD) {
		res = res.add_modifier(ratatui::style::Modifier::BOLD);
	}
	if style.font_style.contains(FontStyle::ITALIC) {
		res = res.add_modifier(ratatui::style::Modifier::ITALIC);
	}
	if style.font_style.contains(FontStyle::UNDERLINE) {
		res = res.add_modifier(ratatui::style::Modifier::UNDERLINED);
	}

	res
}

enum JobState {
	Request((String, String)),
	Response(SyntaxText),
}

#[derive(Clone, Default)]
pub struct AsyncSyntaxJob {
	state: Arc<Mutex<Option<JobState>>>,
	syntax: String,
	line_numbers: bool,
}

impl AsyncSyntaxJob {
	pub fn new(
		content: String,
		path: String,
		syntax: String,
	) -> Self {
		Self {
			state: Arc::new(Mutex::new(Some(JobState::Request((
				content, path,
			))))),
			syntax,
			line_numbers: false,
		}
	}

	/// Enable line numbers in the `bat` output. Only affects the
	/// `bat` path; the `syntect` fallback ignores this.
	pub const fn with_line_numbers(mut self, enable: bool) -> Self {
		self.line_numbers = enable;
		self
	}

	///
	pub fn result(&self) -> Option<SyntaxText> {
		if let Ok(mut state) = self.state.lock() {
			if let Some(state) = state.take() {
				return match state {
					JobState::Request(_) => None,
					JobState::Response(text) => Some(text),
				};
			}
		}

		None
	}
}

impl AsyncJob for AsyncSyntaxJob {
	type Notification = AsyncAppNotification;
	type Progress = ProgressPercent;

	fn run(
		&mut self,
		params: RunParams<Self::Notification, Self::Progress>,
	) -> asyncgit::Result<Self::Notification> {
		let mut state_mutex = self.state.lock()?;

		if let Some(state) = state_mutex.take() {
			*state_mutex = Some(match state {
				JobState::Request((content, path)) => {
					let syntax =
						try_bat(&content, &path, self.line_numbers)
							.unwrap_or_else(|| {
								SyntaxText::new(
									content,
									Path::new(&path),
									&params,
									&self.syntax,
								)
								.unwrap_or_else(|e| {
									log::error!(
									"syntect highlight failed: {e}"
								);
									SyntaxText::from_ansi(
										Vec::new(),
										PathBuf::from(&path),
									)
								})
							});
					JobState::Response(syntax)
				}
				JobState::Response(res) => JobState::Response(res),
			});
		}

		Ok(AsyncAppNotification::SyntaxHighlighting(
			SyntaxHighlightProgress::Done,
		))
	}
}

/// Try to highlight `content` via `bat` (looks for `bat` then `batcat` on
/// PATH). Returns `None` on any failure so the caller falls back to the
/// built-in `syntect` highlighter. `bat` reads `$BAT_THEME` itself — no
/// wiring needed here. When `line_numbers` is true, bat emits a
/// right-aligned line-number gutter (e.g. `···1`) using
/// `--style=plain,numbers` instead of `--plain`.
fn try_bat(
	content: &str,
	path: &str,
	line_numbers: bool,
) -> Option<SyntaxText> {
	let bat = find_in_path(&["bat", "batcat"])?;

	let style_arg = if line_numbers {
		"--style=plain,numbers"
	} else {
		"--plain"
	};

	let mut child = Command::new(bat)
		.args([
			"--color=always",
			style_arg,
			"--paging=never",
			"--file-name",
			path,
		])
		.stdin(Stdio::piped())
		.stdout(Stdio::piped())
		.stderr(Stdio::null())
		.spawn()
		.ok()?;

	let stdin_handle = child.stdin.take();
	let content_owned = content.to_string();
	let stdin_thread = thread::spawn(move || {
		if let Some(mut stdin) = stdin_handle {
			let _ = stdin.write_all(content_owned.as_bytes());
		}
	});

	let output = child.wait_with_output().ok();
	let _ = stdin_thread.join();

	let output = output?;
	if !output.status.success() || output.stdout.is_empty() {
		return None;
	}

	let text = String::from_utf8_lossy(&output.stdout);
	let (lines, _) = crate::ansi::ansi_to_lines(&text);
	if lines.is_empty() {
		return None;
	}

	Some(SyntaxText::from_ansi(lines, PathBuf::from(path)))
}

/// Produce a colored directory listing for the preview pane. Prefers
/// `eza`/`exa`, which can render a recursive tree (names + branches only,
/// via `--tree --level=N`); falls back to a flat `ls -l` listing when
/// neither is installed (plain `ls` has no tree mode). Color is forced
/// even though stdout is piped: `eza --color=always`, GNU `ls
/// --color=always`, BSD `ls -G` with `CLICOLOR_FORCE=1` (macOS). Each
/// candidate is tried in turn so the right one is used regardless of which
/// `ls` flavor is installed. Returns `None` on total failure so the caller
/// can show a message.
///
/// `depth` caps the tree recursion (1 = the folder's immediate children
/// only). It is ignored by the flat `ls` fallback.
pub fn try_dir_listing(
	work_dir: &Path,
	dir: &str,
	depth: u8,
) -> Option<SyntaxText> {
	// The repo root arrives as `""` (the `./` prefix is stripped off `.`).
	// `eza`/`ls` reject an empty path arg, so normalize to `.`.
	let dir = if dir.is_empty() { "." } else { dir };

	if let Some(eza) = find_in_path(&["eza", "exa"]) {
		let level = format!("--level={depth}");
		if let Some(out) = run_lister(
			&eza,
			&["--tree", "--color=always", level.as_str(), dir],
			work_dir,
			None,
			dir,
		) {
			return Some(out);
		}
	}

	if let Some(ls) = find_in_path(&["ls"]) {
		// GNU ls
		if let Some(out) = run_lister(
			&ls,
			&["-l", "--color=always", dir],
			work_dir,
			None,
			dir,
		) {
			return Some(out);
		}
		// BSD ls (macOS): -G enables color, CLICOLOR_FORCE forces it
		// when stdout is not a tty.
		if let Some(out) = run_lister(
			&ls,
			&["-l", "-G", dir],
			work_dir,
			Some(("CLICOLOR_FORCE", "1")),
			dir,
		) {
			return Some(out);
		}
	}

	None
}

/// Spawn `bin` with `args` in `work_dir`, optionally setting one env var,
/// capture stdout, and parse it into a `SyntaxText` via `ansi_to_lines`.
/// `dir` is used as the identity of the resulting `SyntaxText`. Returns
/// `None` if the process fails or produces no output.
fn run_lister(
	bin: &Path,
	args: &[&str],
	work_dir: &Path,
	env: Option<(&str, &str)>,
	dir: &str,
) -> Option<SyntaxText> {
	let mut cmd = Command::new(bin);
	cmd.args(args)
		.current_dir(work_dir)
		.stdout(Stdio::piped())
		.stderr(Stdio::null());

	if let Some((key, val)) = env {
		cmd.env(key, val);
	}

	let output = cmd.output().ok()?;

	if !output.status.success() || output.stdout.is_empty() {
		return None;
	}

	let text = String::from_utf8_lossy(&output.stdout);
	let (lines, _) = crate::ansi::ansi_to_lines(&text);
	if lines.is_empty() {
		return None;
	}

	Some(SyntaxText::from_ansi(lines, PathBuf::from(dir)))
}

/// Whether `bat` (or `batcat`) is on `PATH`. Used to decide whether the
/// preview can defer to a highlighted `bat` result; when true, the plain
/// placeholder is suppressed to avoid a plain→highlighted flash.
pub fn bat_available() -> bool {
	find_in_path(&["bat", "batcat"]).is_some()
}

/// Return the first matching binary found on `PATH`, or `None`.
fn find_in_path(names: &[&str]) -> Option<PathBuf> {
	let path_var = std::env::var_os("PATH")?;
	for dir in std::env::split_paths(&path_var) {
		for name in names {
			let candidate = dir.join(name);
			if candidate.is_file() {
				return Some(candidate);
			}
		}
	}
	None
}
