use crate::{
	app::Environment,
	components::{
		visibility_blocking, CommandBlocking, CommandInfo, Component,
		DrawableComponent, EventState,
	},
	keys::SharedKeyConfig,
	strings,
	ui::{self, style::SharedTheme},
};
use anyhow::{bail, Result};
use asyncgit::sync::{
	get_config_string, utils::repo_work_dir, RepoPath,
};
use crossterm::{
	event::Event,
	terminal::{EnterAlternateScreen, LeaveAlternateScreen},
	ExecutableCommand,
};
use ratatui::{
	layout::Rect,
	text::{Line, Span},
	widgets::{Block, BorderType, Borders, Clear},
	Frame,
};
use scopeguard::defer;
use std::{env, io, path::Path, process::Command};

// Parse shell-style arguments, handling quoted strings
fn parse_args(input: &str) -> Vec<String> {
	let mut args = Vec::new();
	let mut current = String::new();
	let mut in_single_quote = false;
	let mut in_double_quote = false;
	let mut chars = input.chars().peekable();

	while let Some(c) = chars.next() {
		if in_single_quote {
			if c == '\'' {
				in_single_quote = false;
			} else {
				current.push(c);
			}
		} else if in_double_quote {
			if c == '"' {
				in_double_quote = false;
			} else if c == '\\' {
				// Handle escape in double quotes
				if let Some(&next) = chars.peek() {
					chars.next();
					current.push(next);
				}
			} else {
				current.push(c);
			}
		} else {
			match c {
				'\'' => {
					in_single_quote = true;
				}
				'"' => {
					in_double_quote = true;
				}
				'\\' => {
					// Handle escape outside quotes
					if let Some(&next) = chars.peek() {
						chars.next();
						current.push(next);
					}
				}
				' ' | '\t' | '\n' | '\r' => {
					if !current.is_empty() {
						args.push(std::mem::take(&mut current));
					}
				}
				_ => {
					current.push(c);
				}
			}
		}
	}

	if !current.is_empty() {
		args.push(current);
	}

	args
}

///
pub struct ExternalEditorPopup {
	visible: bool,
	theme: SharedTheme,
	key_config: SharedKeyConfig,
}

impl ExternalEditorPopup {
	///
	pub fn new(env: &Environment) -> Self {
		Self {
			visible: false,
			theme: env.theme.clone(),
			key_config: env.key_config.clone(),
		}
	}

	/// opens file at given `path` in an available editor
	pub fn open_file_in_editor(
		repo: &RepoPath,
		path: &Path,
	) -> Result<()> {
		let work_dir = repo_work_dir(repo)?;

		let path = if path.is_relative() {
			Path::new(&work_dir).join(path)
		} else {
			path.into()
		};

		if !path.exists() {
			bail!("file not found: {path:?}");
		}

		io::stdout().execute(LeaveAlternateScreen)?;
		defer! {
			io::stdout().execute(EnterAlternateScreen).expect("reset terminal");
		}

		let environment_options = ["GIT_EDITOR", "EDITOR"];

		let editor = env::var(environment_options[0])
			.ok()
			.or_else(|| {
				get_config_string(repo, "core.editor").ok()?
			})
			.or_else(|| env::var(environment_options[1]).ok())
			.unwrap_or_else(|| String::from("vi"));

		let all_args = parse_args(&editor);

		let command = all_args.first().ok_or_else(|| {
			anyhow::anyhow!(
				"editor env variable found empty: {}",
				environment_options.join(" or ")
			)
		})?;

		let args: Vec<&std::ffi::OsStr> = all_args
			.iter()
			.skip(1)
			.map(std::ffi::OsStr::new)
			.chain(std::iter::once(path.as_os_str()))
			.collect();

		Command::new(command)
			.current_dir(work_dir)
			.args(args)
			.status()
			.map_err(|e| anyhow::anyhow!("\"{command}\": {e}"))?;

		Ok(())
	}
}

impl DrawableComponent for ExternalEditorPopup {
	fn draw(&self, f: &mut Frame, _rect: Rect) -> Result<()> {
		if self.visible {
			let txt = Line::from(
				strings::msg_opening_editor(&self.key_config)
					.split('\n')
					.map(|string| {
						Span::raw::<String>(string.to_string())
					})
					.collect::<Vec<Span>>(),
			);

			let area = ui::centered_rect_absolute(25, 3, f.area());
			f.render_widget(Clear, area);
			let block = Block::default()
				.borders(Borders::ALL)
				.border_type(BorderType::Thick)
				.border_style(self.theme.block(true))
				.style(self.theme.block(true));
			ui::render_block_text(
				f.buffer_mut(),
				area,
				block,
				std::iter::once(&txt),
			);
		}

		Ok(())
	}
}

impl Component for ExternalEditorPopup {
	fn commands(
		&self,
		out: &mut Vec<CommandInfo>,
		force_all: bool,
	) -> CommandBlocking {
		if self.visible && !force_all {
			out.clear();
		}

		visibility_blocking(self)
	}

	fn event(&mut self, _ev: &Event) -> Result<EventState> {
		if self.visible {
			return Ok(EventState::Consumed);
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

		Ok(())
	}
}
