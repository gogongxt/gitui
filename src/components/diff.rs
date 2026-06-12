use super::{
	utils::scroll_horizontal::HorizontalScroll,
	utils::scroll_vertical::VerticalScroll, CommandBlocking,
	Direction, DrawableComponent, HorizontalScrollType, ScrollType,
};
use crate::{
	app::Environment,
	components::{CommandInfo, Component, EventState},
	keys::{key_match, GituiKeyEvent, SharedKeyConfig},
	options::SharedOptions,
	queue::{Action, InternalEvent, NeedsUpdate, Queue, ResetItem},
	string_utils::tabs_to_spaces,
	string_utils::trim_offset,
	strings, try_or_popup,
	ui::style::SharedTheme,
};
use anyhow::Result;
use asyncgit::{
	hash,
	sync::{self, diff::DiffLinePosition, RepoPathRef},
	DiffLine, DiffLineType, DiffType, FileDiff,
};
use bytesize::ByteSize;
use crossterm::event::{Event, KeyCode, KeyModifiers};
use ratatui::{
	layout::{
		Constraint, Direction as RatatuiDirection, Layout, Rect,
	},
	symbols,
	text::{Line, Span},
	widgets::{Block, Borders, Paragraph},
	Frame,
};
use serde::{Deserialize, Serialize};
use std::{
	borrow::Cow,
	cell::{Cell, RefCell},
	cmp,
	collections::HashMap,
	path::Path,
};

/// Diff display mode
#[derive(
	Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize,
)]
pub enum DiffMode {
	#[default]
	Unified,
	SideBySide,
	Delta,
	DeltaSideBySide,
}

struct Current {
	path: String,
	is_stage: bool,
	hash: u64,
	diff_type: DiffType,
}

impl Default for Current {
	fn default() -> Self {
		Self {
			path: String::new(),
			is_stage: false,
			hash: 0,
			diff_type: DiffType::WorkDir,
		}
	}
}

///
#[derive(Clone, Copy)]
enum Selection {
	Single(usize),
	Multiple(usize, usize),
}

impl Selection {
	const fn get_start(&self) -> usize {
		match self {
			Self::Single(start) | Self::Multiple(start, _) => *start,
		}
	}

	const fn get_end(&self) -> usize {
		match self {
			Self::Single(end) | Self::Multiple(_, end) => *end,
		}
	}

	fn get_top(&self) -> usize {
		match self {
			Self::Single(start) => *start,
			Self::Multiple(start, end) => cmp::min(*start, *end),
		}
	}

	fn get_bottom(&self) -> usize {
		match self {
			Self::Single(start) => *start,
			Self::Multiple(start, end) => cmp::max(*start, *end),
		}
	}

	fn modify(&mut self, direction: Direction, max: usize) {
		let start = self.get_start();
		let old_end = self.get_end();

		*self = match direction {
			Direction::Up => {
				Self::Multiple(start, old_end.saturating_sub(1))
			}

			Direction::Down => {
				Self::Multiple(start, cmp::min(old_end + 1, max))
			}
		};
	}

	fn contains(&self, index: usize) -> bool {
		match self {
			Self::Single(start) => index == *start,
			Self::Multiple(start, end) => {
				if start <= end {
					*start <= index && index <= *end
				} else {
					*end <= index && index <= *start
				}
			}
		}
	}
}

/// A single line in side-by-side diff view
struct SideBySideLine {
	left_content: String,
	left_line_num: Option<u32>,
	right_content: String,
	right_line_num: Option<u32>,
	left_type: DiffLineType,
	right_type: DiffLineType,
	/// Global line index for selection tracking
	global_line_idx: usize,
	/// Index of the hunk this line belongs to
	hunk_idx: usize,
	/// Whether this is the first line of a hunk
	is_hunk_start: bool,
	/// Whether this is the last line of a hunk
	is_hunk_end: bool,
}

///
pub struct DiffComponent {
	repo: RepoPathRef,
	diff: Option<FileDiff>,
	longest_line: Cell<usize>,
	pending: bool,
	selection: Selection,
	selected_hunk: Option<usize>,
	current_size: Cell<(u16, u16)>,
	focused: bool,
	current: Current,
	vertical_scroll: VerticalScroll,
	horizontal_scroll: HorizontalScroll,
	queue: Queue,
	theme: SharedTheme,
	key_config: SharedKeyConfig,
	is_immutable: bool,
	options: SharedOptions,
	diff_mode: DiffMode,
	delta_output: RefCell<Option<Vec<Line<'static>>>>,
	delta_line_hunks: RefCell<Vec<usize>>,
	delta_line_positions: RefCell<Vec<Option<DiffLinePosition>>>,
	last_delta_width: Cell<u16>,
}

impl DiffComponent {
	///
	pub fn new(env: &Environment, is_immutable: bool) -> Self {
		Self {
			focused: false,
			queue: env.queue.clone(),
			current: Current::default(),
			pending: false,
			selected_hunk: None,
			diff: None,
			longest_line: Cell::new(0),
			current_size: Cell::new((0, 0)),
			selection: Selection::Single(0),
			vertical_scroll: VerticalScroll::new(),
			horizontal_scroll: HorizontalScroll::new(),
			theme: env.theme.clone(),
			key_config: env.key_config.clone(),
			is_immutable,
			repo: env.repo.clone(),
			options: env.options.clone(),
			diff_mode: env.options.borrow().diff_mode(),
			delta_output: RefCell::new(None),
			delta_line_hunks: RefCell::new(Vec::new()),
			delta_line_positions: RefCell::new(Vec::new()),
			last_delta_width: Cell::new(0),
		}
	}
	///
	fn can_scroll(&self) -> bool {
		if self.is_delta_preview() {
			return self
				.delta_output
				.borrow()
				.as_ref()
				.is_some_and(|l| l.len() > 1);
		}
		self.diff.as_ref().is_some_and(|diff| diff.lines > 1)
	}
	///
	pub fn current(&self) -> (String, bool) {
		(self.current.path.clone(), self.current.is_stage)
	}
	///
	pub fn clear(&mut self, pending: bool) {
		self.current = Current::default();
		self.diff = None;
		*self.delta_output.borrow_mut() = None;
		self.delta_line_hunks.borrow_mut().clear();
		self.delta_line_positions.borrow_mut().clear();
		self.longest_line.set(0);
		self.vertical_scroll.reset();
		self.horizontal_scroll.reset();
		self.selection = Selection::Single(0);
		self.selected_hunk = None;
		self.pending = pending;
	}
	///
	pub fn update(
		&mut self,
		path: String,
		is_stage: bool,
		diff: FileDiff,
		diff_type: DiffType,
	) {
		self.pending = false;

		let hash = hash(&diff);

		if self.current.hash != hash {
			let reset_selection = self.current.path != path;

			self.current = Current {
				path,
				is_stage,
				hash,
				diff_type,
			};

			self.diff = Some(diff);

			self.longest_line.set(
				self.diff
					.iter()
					.flat_map(|diff| diff.hunks.iter())
					.flat_map(|hunk| hunk.lines.iter())
					.map(|line| {
						let converted_content = tabs_to_spaces(
							line.content.as_ref().to_string(),
						);

						converted_content.len()
					})
					.max()
					.map_or(0, |len| {
						// Each hunk uses a 1-character wide vertical bar to its left to indicate
						// selection.
						len + 1
					}),
			);

			if self.is_delta_preview() {
				// In delta mode, preserve selection and rebuild delta maps after
				if reset_selection {
					self.vertical_scroll.reset();
					self.selection = Selection::Single(0);
				}
				self.run_delta();
				// Update longest_line from delta output for horizontal scrolling
				self.longest_line.set(
					self.delta_output.borrow().as_ref().map_or(
						0,
						|lines| {
							lines
								.iter()
								.map(|line| {
									line.spans
										.iter()
										.map(|s| {
											s.content.chars().count()
										})
										.sum::<usize>()
								})
								.max()
								.unwrap_or(0)
						},
					),
				);
				// Clamp selection to new delta line count
				let max = self
					.delta_output
					.borrow()
					.as_ref()
					.map_or(0, |l| l.len().saturating_sub(1));
				if let Selection::Single(line) = &self.selection {
					if *line > max {
						self.selection = Selection::Single(max);
					}
				}
				// Update selected_hunk from delta hunk mapping
				let idx = self.selection.get_end();
				let hunk_map = self.delta_line_hunks.borrow();
				let max_hunk = self
					.diff
					.as_ref()
					.map_or(0, |d| d.hunks.len().saturating_sub(1));
				self.selected_hunk = hunk_map
					.get(idx)
					.copied()
					.map(|h| h.min(max_hunk));
			} else if reset_selection {
				self.vertical_scroll.reset();
				self.selection = Selection::Single(0);
				self.update_selection(0);
			} else {
				let old_selection = match self.selection {
					Selection::Single(line) => line,
					Selection::Multiple(start, _) => start,
				};
				self.update_selection(old_selection);
			}
		}
	}

	fn move_selection(&mut self, move_type: ScrollType) {
		// In delta mode, scroll based on delta_output lines
		if self.is_delta_preview() {
			let max = self
				.delta_output
				.borrow()
				.as_ref()
				.map_or(0, |l| l.len().saturating_sub(1));
			let new_start = match move_type {
				ScrollType::Down => {
					let next =
						self.selection.get_bottom().saturating_add(1);
					cmp::min(next, max)
				}
				ScrollType::Up => {
					self.selection.get_top().saturating_sub(1)
				}
				ScrollType::Home => 0,
				ScrollType::End => max,
				ScrollType::PageDown => {
					let next =
						self.selection.get_bottom().saturating_add(
							self.current_size
								.get()
								.1
								.saturating_sub(1) as usize,
						);
					cmp::min(next, max)
				}
				ScrollType::PageUp => {
					self.selection.get_top().saturating_sub(
						self.current_size.get().1.saturating_sub(1)
							as usize,
					)
				}
			};
			self.selection = Selection::Single(new_start);
			// Update selected_hunk from delta hunk mapping
			let hunk_map = self.delta_line_hunks.borrow();
			let max_hunk = self
				.diff
				.as_ref()
				.map_or(0, |d| d.hunks.len().saturating_sub(1));
			self.selected_hunk = hunk_map
				.get(new_start)
				.copied()
				.map(|h| h.min(max_hunk));
			return;
		}

		if let Some(diff) = &self.diff {
			// In side-by-side mode, display lines differ from diff.lines
			// because Delete+Add pairs are shown as one line
			let max = if self.diff_mode == DiffMode::SideBySide {
				self.side_by_side_lines_count().saturating_sub(1)
			} else {
				diff.lines.saturating_sub(1)
			};

			let new_start = match move_type {
				ScrollType::Down => {
					let next =
						self.selection.get_bottom().saturating_add(1);
					cmp::min(next, max)
				}
				ScrollType::Up => {
					self.selection.get_top().saturating_sub(1)
				}
				ScrollType::Home => 0,
				ScrollType::End => max,
				ScrollType::PageDown => {
					let next =
						self.selection.get_bottom().saturating_add(
							self.current_size
								.get()
								.1
								.saturating_sub(1) as usize,
						);
					cmp::min(next, max)
				}
				ScrollType::PageUp => {
					self.selection.get_top().saturating_sub(
						self.current_size.get().1.saturating_sub(1)
							as usize,
					)
				}
			};

			self.update_selection(new_start);
		}
	}

	fn update_selection(&mut self, new_start: usize) {
		if let Some(diff) = &self.diff {
			// In side-by-side mode, display lines differ from diff.lines
			let max = if self.diff_mode == DiffMode::SideBySide {
				self.side_by_side_lines_count().saturating_sub(1)
			} else {
				diff.lines.saturating_sub(1)
			};
			let new_start = cmp::min(max, new_start);
			self.selection = Selection::Single(new_start);
			self.selected_hunk =
				Self::find_selected_hunk_for_display_line(
					diff,
					new_start,
					self.diff_mode,
				);
		}
	}

	fn lines_count(&self) -> usize {
		if self.is_delta_preview() {
			return self
				.delta_output
				.borrow()
				.as_ref()
				.map_or(0, Vec::len);
		}
		self.diff.as_ref().map_or(0, |diff| diff.lines)
	}

	/// Get the actual display line count for side-by-side mode
	/// In side-by-side mode, Delete+Add pairs are shown as one line
	fn side_by_side_lines_count(&self) -> usize {
		let Some(diff) = &self.diff else {
			return 0;
		};

		if diff.hunks.is_empty() {
			return 0;
		}

		let mut count = 0_usize;
		for hunk in &diff.hunks {
			let mut i = 0;
			while i < hunk.lines.len() {
				let line = &hunk.lines[i];
				if line.line_type == DiffLineType::Delete {
					// Check if next line is Add (they will be paired)
					if let Some(next) = hunk.lines.get(i + 1) {
						if next.line_type == DiffLineType::Add {
							i += 1; // Skip the Add line in counting
						}
					}
				}
				count += 1;
				i += 1;
			}
		}

		count
	}

	fn max_scroll_right(&self) -> usize {
		let line_num_width: u16 =
			self.get_line_num_width().try_into().unwrap_or(u16::MAX);
		let available_width: usize =
			if self.diff_mode == DiffMode::SideBySide {
				// In side-by-side mode, each panel's content width:
				// chunks[0].width ≈ r.width / 2
				// content width = chunks[0].width - (border + marker + line_num_width + space)
				// current_width = r.width - 2
				// overhead = 2 (borders) + 1 (marker) + line_num_width + 1 (space)
				(self.current_size.get().0 / 2)
					.saturating_sub(2 + 1 + line_num_width + 1)
					.into()
			} else if self.is_delta_preview() {
				// In delta mode, line numbers are already in delta output
				// Only subtract borders
				usize::from(self.current_size.get().0)
			} else {
				// In unified mode, we have two line number columns
				// overhead = 1 (marker) + line_num_width * 2 + 1 (space between line numbers) + 1 (space after line numbers)
				let line_num_overhead = line_num_width * 2 + 3;
				self.current_size
					.get()
					.0
					.saturating_sub(line_num_overhead)
					.into()
			};
		self.longest_line.get().saturating_sub(available_width)
	}

	fn modify_selection(&mut self, direction: Direction) {
		if self.diff.is_some() || self.is_delta_preview() {
			self.selection.modify(direction, self.lines_count());
		}
	}

	fn copy_selection(&self) {
		if let Some(diff) = &self.diff {
			let lines_to_copy: Vec<&str> =
				diff.hunks
					.iter()
					.flat_map(|hunk| hunk.lines.iter())
					.enumerate()
					.filter_map(|(i, line)| {
						if self.selection.contains(i) {
							Some(line.content.trim_matches(|c| {
								c == '\n' || c == '\r'
							}))
						} else {
							None
						}
					})
					.collect();

			try_or_popup!(
				self,
				"copy to clipboard error:",
				crate::clipboard::copy_string(
					&lines_to_copy.join("\n")
				)
			);
		}
	}

	fn find_selected_hunk(
		diff: &FileDiff,
		line_selected: usize,
	) -> Option<usize> {
		let mut line_cursor = 0_usize;
		for (i, hunk) in diff.hunks.iter().enumerate() {
			let hunk_len = hunk.lines.len();
			let hunk_min = line_cursor;
			let hunk_max = line_cursor + hunk_len;

			let hunk_selected =
				hunk_min <= line_selected && hunk_max > line_selected;

			if hunk_selected {
				return Some(i);
			}

			line_cursor += hunk_len;
		}

		None
	}

	/// Find the hunk index for a display line (accounting for side-by-side pairing)
	fn find_selected_hunk_for_display_line(
		diff: &FileDiff,
		display_line_selected: usize,
		diff_mode: DiffMode,
	) -> Option<usize> {
		if diff_mode == DiffMode::Unified {
			return Self::find_selected_hunk(
				diff,
				display_line_selected,
			);
		}

		// For side-by-side mode, count display lines (where Delete+Add pairs count as 1)
		let mut display_cursor = 0_usize;
		for (i, hunk) in diff.hunks.iter().enumerate() {
			let mut j = 0;
			let hunk_start = display_cursor;
			while j < hunk.lines.len() {
				let line = &hunk.lines[j];
				if display_cursor == display_line_selected {
					return Some(i);
				}
				if line.line_type == DiffLineType::Delete {
					if let Some(next) = hunk.lines.get(j + 1) {
						if next.line_type == DiffLineType::Add {
							j += 1;
						}
					}
				}
				display_cursor += 1;
				j += 1;
			}
			// Check if this is the last line of the hunk
			if display_line_selected >= hunk_start
				&& display_line_selected < display_cursor
			{
				return Some(i);
			}
		}

		None
	}

	fn get_text(&self, width: u16, height: u16) -> Vec<Line<'_>> {
		if let Some(diff) = &self.diff {
			return if diff.hunks.is_empty() {
				self.get_text_binary(diff)
			} else {
				let mut res: Vec<Line> = Vec::new();

				let min = self.vertical_scroll.get_top();
				let max = min + height as usize;

				let mut line_cursor = 0_usize;
				let mut lines_added = 0_usize;

				let line_num_width = self.get_line_num_width();

				for (i, hunk) in diff.hunks.iter().enumerate() {
					let hunk_selected = self.focused()
						&& self.selected_hunk.is_some_and(|s| s == i);

					if lines_added >= height as usize {
						break;
					}

					let hunk_len = hunk.lines.len();
					let hunk_min = line_cursor;
					let hunk_max = line_cursor + hunk_len;

					if Self::hunk_visible(
						hunk_min, hunk_max, min, max,
					) {
						for (i, line) in hunk.lines.iter().enumerate()
						{
							if line_cursor >= min
								&& line_cursor <= max
							{
								res.push(Self::get_line_to_add(
									width,
									line,
									self.focused()
										&& self
											.selection
											.contains(line_cursor),
									hunk_selected,
									i == hunk_len - 1,
									&self.theme,
									self.horizontal_scroll
										.get_right(),
									line_num_width,
								));
								lines_added += 1;
							}

							line_cursor += 1;
						}
					} else {
						line_cursor += hunk_len;
					}
				}

				res
			};
		}

		vec![]
	}

	fn get_text_binary(&self, diff: &FileDiff) -> Vec<Line<'_>> {
		let is_positive = diff.size_delta >= 0;
		let delta_byte_size =
			ByteSize::b(diff.size_delta.unsigned_abs());
		let sign = if is_positive { "+" } else { "-" };
		vec![Line::from(vec![
			Span::raw(Cow::from("size: ")),
			Span::styled(
				Cow::from(format!("{}", ByteSize::b(diff.sizes.0))),
				self.theme.text(false, false),
			),
			Span::raw(Cow::from(" -> ")),
			Span::styled(
				Cow::from(format!("{}", ByteSize::b(diff.sizes.1))),
				self.theme.text(false, false),
			),
			Span::raw(Cow::from(" (")),
			Span::styled(
				Cow::from(format!("{sign}{delta_byte_size:}")),
				self.theme.diff_line(
					if is_positive {
						DiffLineType::Add
					} else {
						DiffLineType::Delete
					},
					false,
				),
			),
			Span::raw(Cow::from(")")),
		])]
	}

	#[allow(clippy::too_many_arguments)]
	fn get_line_to_add<'a>(
		width: u16,
		line: &'a DiffLine,
		selected: bool,
		selected_hunk: bool,
		end_of_hunk: bool,
		theme: &SharedTheme,
		scrolled_right: usize,
		line_num_width: usize,
	) -> Line<'a> {
		let style = theme.diff_hunk_marker(selected_hunk);

		let is_content_line =
			matches!(line.line_type, DiffLineType::None);

		let left_side_of_line = if end_of_hunk {
			Span::styled(Cow::from(symbols::line::BOTTOM_LEFT), style)
		} else {
			match line.line_type {
				DiffLineType::Header => Span::styled(
					Cow::from(symbols::line::TOP_LEFT),
					style,
				),
				_ => Span::styled(
					Cow::from(symbols::line::VERTICAL),
					style,
				),
			}
		};

		// Format line numbers in two columns (like GitHub)
		let old_line_num = line.position.old_lineno;
		let new_line_num = line.position.new_lineno;

		let old_line_str = old_line_num.map_or_else(
			|| " ".repeat(line_num_width),
			|n| format!("{n:line_num_width$}"),
		);
		let new_line_str = new_line_num.map_or_else(
			|| " ".repeat(line_num_width),
			|n| format!("{n:line_num_width$}"),
		);

		let line_numbers = format!("{old_line_str} {new_line_str} ");

		let content =
			if !is_content_line && line.content.as_ref().is_empty() {
				theme.line_break()
			} else {
				tabs_to_spaces(line.content.as_ref().to_string())
			};
		let content = trim_offset(&content, scrolled_right);

		// Adjust width to account for line numbers
		let line_num_overhead: u16 = (line_num_width * 2 + 2) // Two line numbers + space separator
			.try_into()
			.unwrap_or(u16::MAX);
		let content_width: usize =
			width.saturating_sub(line_num_overhead).into();

		let filled = if selected {
			// selected line
			format!("{content:content_width$}\n")
		} else {
			// weird eof missing eol line
			format!("{content}\n")
		};

		Line::from(vec![
			left_side_of_line,
			Span::styled(
				Cow::from(line_numbers),
				theme.text(false, false),
			),
			Span::styled(
				Cow::from(filled),
				theme.diff_line(line.line_type, selected),
			),
		])
	}

	const fn hunk_visible(
		hunk_min: usize,
		hunk_max: usize,
		min: usize,
		max: usize,
	) -> bool {
		// full overlap
		if hunk_min <= min && hunk_max >= max {
			return true;
		}

		// partly overlap
		if (hunk_min >= min && hunk_min <= max)
			|| (hunk_max >= min && hunk_max <= max)
		{
			return true;
		}

		false
	}

	fn unstage_hunk(&self) -> Result<()> {
		if let Some(diff) = &self.diff {
			if let Some(hunk) = self.selected_hunk {
				if let Some(hunk_data) = diff.hunks.get(hunk) {
					sync::unstage_hunk(
						&self.repo.borrow(),
						&self.current.path,
						hunk_data.header_hash,
						Some(self.options.borrow().diff_options()),
					)?;
					self.queue_update();
				}
			}
		}

		Ok(())
	}

	fn stage_hunk(&self) -> Result<()> {
		if let Some(diff) = &self.diff {
			if let Some(hunk) = self.selected_hunk {
				if diff.untracked {
					sync::stage_add_file(
						&self.repo.borrow(),
						Path::new(&self.current.path),
					)?;
				} else if let Some(hunk_data) = diff.hunks.get(hunk) {
					sync::stage_hunk(
						&self.repo.borrow(),
						&self.current.path,
						hunk_data.header_hash,
						Some(self.options.borrow().diff_options()),
					)?;
				}

				self.queue_update();
			}
		}

		Ok(())
	}

	fn queue_update(&self) {
		self.queue.push(InternalEvent::Update(NeedsUpdate::ALL));
	}

	fn reset_hunk(&self) {
		if let Some(diff) = &self.diff {
			if let Some(hunk) = self.selected_hunk {
				let hash = diff.hunks[hunk].header_hash;

				self.queue.push(InternalEvent::ConfirmAction(
					Action::ResetHunk(
						self.current.path.clone(),
						hash,
					),
				));
			}
		}
	}

	fn reset_lines(&self) {
		self.queue.push(InternalEvent::ConfirmAction(
			Action::ResetLines(
				self.current.path.clone(),
				self.selected_lines(),
			),
		));
	}

	fn stage_lines(&self) {
		if let Some(diff) = &self.diff {
			//TODO: support untracked files as well
			if !diff.untracked {
				let selected_lines = self.selected_lines();

				try_or_popup!(
					self,
					"(un)stage lines:",
					sync::stage_lines(
						&self.repo.borrow(),
						&self.current.path,
						self.is_stage(),
						&selected_lines,
					)
				);

				self.queue_update();
			}
		}
	}

	fn selected_lines(&self) -> Vec<DiffLinePosition> {
		if self.is_delta_preview() {
			let positions = self.delta_line_positions.borrow();
			let sel = self.selection.get_end();
			let result: Vec<DiffLinePosition> = (0..positions.len())
				.filter(|&i| self.selection.contains(i))
				.filter_map(|i| positions[i])
				.collect();
			log::debug!(
				"delta selected_lines: sel={}, positions_len={}, has_pos={}, result_len={}",
				sel,
				positions.len(),
				positions.get(sel).is_some_and(Option::is_some),
				result.len()
			);
			return result;
		}
		self.diff
			.as_ref()
			.map(|diff| {
				diff.hunks
					.iter()
					.flat_map(|hunk| hunk.lines.iter())
					.enumerate()
					.filter_map(|(i, line)| {
						let is_add_or_delete = line.line_type
							== DiffLineType::Add
							|| line.line_type == DiffLineType::Delete;
						if self.selection.contains(i)
							&& is_add_or_delete
						{
							Some(line.position)
						} else {
							None
						}
					})
					.collect()
			})
			.unwrap_or_default()
	}

	fn reset_untracked(&self) {
		self.queue.push(InternalEvent::ConfirmAction(Action::Reset(
			ResetItem {
				path: self.current.path.clone(),
			},
		)));
	}

	fn stage_unstage_hunk(&self) -> Result<()> {
		if self.current.is_stage {
			self.unstage_hunk()?;
		} else {
			self.stage_hunk()?;
		}

		Ok(())
	}

	fn calc_hunk_move_target(
		&self,
		direction: isize,
	) -> Option<usize> {
		let diff = self.diff.as_ref()?;
		if diff.hunks.is_empty() {
			return None;
		}
		let max = diff.hunks.len() - 1;
		let target_index = self.selected_hunk.map_or(0, |i| {
			let target = if direction >= 0 {
				i.saturating_add(direction.unsigned_abs())
			} else {
				i.saturating_sub(direction.unsigned_abs())
			};
			std::cmp::min(max, target)
		});
		Some(target_index)
	}

	fn diff_hunk_move_up_down(&mut self, direction: isize) {
		let Some(diff) = &self.diff else { return };
		let hunk_index = self.calc_hunk_move_target(direction);
		// return if selected_hunk not change
		if self.selected_hunk == hunk_index {
			return;
		}
		if let Some(hunk_index) = hunk_index {
			if self.is_delta_preview() {
				// In delta mode, find the delta output line range for this hunk
				let hunk_map = self.delta_line_hunks.borrow();
				let delta_start = hunk_map
					.iter()
					.position(|&h| h == hunk_index)
					.unwrap_or(0);
				// Find end: next hunk's start or end of all lines
				let delta_end = hunk_map
					.iter()
					.skip(delta_start)
					.position(|&h| h > hunk_index)
					.map_or(hunk_map.len(), |p| delta_start + p);
				self.selection = Selection::Single(delta_start);
				self.selected_hunk = Some(hunk_index);
				self.vertical_scroll.move_area_to_visible(
					self.current_size.get().1 as usize,
					delta_start,
					delta_end,
				);
			} else {
				let line_index = diff
					.hunks
					.iter()
					.take(hunk_index)
					.fold(0, |sum, hunk| sum + hunk.lines.len());
				let hunk = &diff.hunks[hunk_index];
				self.selection = Selection::Single(line_index);
				self.selected_hunk = Some(hunk_index);
				self.vertical_scroll.move_area_to_visible(
					self.current_size.get().1 as usize,
					line_index,
					line_index.saturating_add(hunk.lines.len()),
				);
			}
		}
	}

	/// Returns `true` if current mode is any delta preview
	const fn is_delta_preview(&self) -> bool {
		match self.diff_mode {
			DiffMode::Delta | DiffMode::DeltaSideBySide => true,
			DiffMode::Unified | DiffMode::SideBySide => false,
		}
	}

	/// Check if the `delta` binary is available on PATH
	fn is_delta_available() -> bool {
		std::process::Command::new("delta")
			.arg("--version")
			.stdout(std::process::Stdio::null())
			.stderr(std::process::Stdio::null())
			.status()
			.is_ok_and(|s| s.success())
	}

	/// Run `git diff | delta` and return parsed lines, or `None` on failure.
	#[allow(clippy::too_many_lines, clippy::cognitive_complexity)]
	fn run_delta_subprocess(
		repo: &RepoPathRef,
		path: &str,
		diff_type: &DiffType,
		width: u16,
		side_by_side: bool,
	) -> Option<Vec<Line<'static>>> {
		if path.is_empty() {
			return None;
		}

		let repo_ref = repo.borrow();
		let work_dir = repo_ref
			.workdir()
			.map(std::path::Path::to_path_buf)
			.or_else(|| {
				let output = std::process::Command::new("git")
					.args(["rev-parse", "--show-toplevel"])
					.current_dir(repo_ref.gitpath())
					.output()
					.ok()?;
				if output.status.success() {
					let path =
						String::from_utf8_lossy(&output.stdout);
					Some(std::path::PathBuf::from(path.trim()))
				} else {
					None
				}
			});
		drop(repo_ref);

		let work_dir = work_dir?;

		let mut git_cmd = std::process::Command::new("git");
		match diff_type {
			DiffType::WorkDir => {
				git_cmd.arg("diff");
				git_cmd.arg(path);
			}
			DiffType::Stage => {
				git_cmd.arg("diff");
				git_cmd.arg("--cached");
				git_cmd.arg(path);
			}
			DiffType::Commit(commit_id) => {
				git_cmd.arg("diff");
				// Use parent..commit to show changes IN the commit.
				// For the first commit (no parent), fall back to empty tree.
				let range = format!("{commit_id}^..{commit_id}");
				git_cmd.arg(&range);
				git_cmd.arg("--");
				git_cmd.arg(path);
			}
			DiffType::Commits(ids) => {
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
			// For commit diffs, the ^.. syntax fails on the first commit (no parent).
			// Fall back to diffing against the empty tree.
			if let DiffType::Commit(commit_id) = diff_type {
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
				"delta: git diff produced empty output for {diff_type:?} {path}"
			);
		}

		// For untracked files, git diff produces empty output.
		// Fall back to diffing against /dev/null to show the full file as added.
		let git_output = if git_output.stdout.is_empty()
			&& matches!(diff_type, DiffType::WorkDir)
		{
			let fallback = std::process::Command::new("git")
				.args(["diff", "--no-index", "/dev/null", path])
				.current_dir(&work_dir)
				.output()
				.ok()?;
			if fallback.status.success()
				|| fallback.status.code() == Some(1)
			{
				// diff --no-index exits 1 when files differ
				fallback
			} else {
				git_output
			}
		} else {
			git_output
		};

		if git_output.stdout.is_empty() {
			log::debug!(
				"delta: no diff content for {diff_type:?} {path}"
			);
			return None;
		}

		let width = width.max(20);
		let mut delta_args = vec![
			"--width".to_string(),
			width.to_string(),
			"--file-style".to_string(),
			"omit".to_string(),
			"--line-numbers".to_string(),
		];
		if side_by_side {
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
			if let Some(stdin) = child.stdin.take() {
				use std::io::Write;
				let mut stdin = stdin;
				let _ = stdin.write_all(&git_output.stdout);
			}
			child
				.wait_with_output()
				.ok()
				.filter(|o| o.status.success())
				.map(|o| o.stdout)
		} else {
			log::error!("delta: failed to spawn delta process");
			None
		};

		if delta_bytes.is_none() {
			log::error!("delta: delta process failed or produced no output for {diff_type:?} {path}");
		}

		delta_bytes.map(|bytes| {
			let text = String::from_utf8_lossy(&bytes);
			crate::ansi::ansi_to_lines(&text)
		})
	}

	/// Run delta and store the result
	fn run_delta(&self) {
		let output = Self::run_delta_subprocess(
			&self.repo,
			&self.current.path,
			&self.current.diff_type,
			self.current_size.get().0,
			self.diff_mode == DiffMode::DeltaSideBySide,
		);
		*self.delta_output.borrow_mut() = output;
		self.rebuild_delta_maps();
		self.last_delta_width.set(self.current_size.get().0);
	}

	/// Build mappings from delta display lines to hunk indices and diff line positions.
	fn rebuild_delta_maps(&self) {
		let mut hunk_map = Vec::new();
		let mut pos_map: Vec<Option<DiffLinePosition>> = Vec::new();
		let delta = self.delta_output.borrow();

		// Build two lookups: one for old_lineno (delete lines), one for new_lineno (add/context lines)
		// Delta non-side-by-side format: `old_lineno ⋮ new_lineno │ content`
		// - Delete lines have old_lineno only (new_lineno is empty)
		// - Add lines have new_lineno only (old_lineno is empty)
		// - Context lines have both
		let (old_lineno_lookup, new_lineno_lookup): (
			HashMap<u32, DiffLinePosition>,
			HashMap<u32, DiffLinePosition>,
		) = self.diff.as_ref().map_or_else(
			|| (HashMap::new(), HashMap::new()),
			|diff| {
				let mut old_map = HashMap::new();
				let mut new_map = HashMap::new();
				for hunk in &diff.hunks {
					for line in &hunk.lines {
						match line.line_type {
							DiffLineType::Add => {
								if let Some(n) =
									line.position.new_lineno
								{
									new_map.insert(n, line.position);
								}
							}
							DiffLineType::Delete => {
								if let Some(n) =
									line.position.old_lineno
								{
									old_map.insert(n, line.position);
								}
							}
							_ => {
								// Context lines: map both old and new
								if let Some(n) =
									line.position.old_lineno
								{
									old_map
										.entry(n)
										.or_insert(line.position);
								}
								if let Some(n) =
									line.position.new_lineno
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

		if let Some(lines) = delta.as_ref() {
			let mut current_hunk: usize = 0;
			let mut first_separator_seen = false;
			for line in lines {
				let text: String = line
					.spans
					.iter()
					.map(|s| s.content.as_ref())
					.collect();
				let trimmed = text.trim();

				// Hunk mapping
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

				// Line position mapping: parse old and new line numbers from gutter spans
				// Delta non-side-by-side format: `old_lineno ⋮ new_lineno │ content`
				let (old_lineno, new_lineno) =
					Self::parse_delta_line_numbers(line);
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
		let pos_count =
			pos_map.iter().filter(|p| p.is_some()).count();
		log::debug!(
			"rebuild_delta_maps: {} lines, {} hunks, {} positions with line numbers",
			hunk_map.len(),
			hunk_map.iter().max().map_or(0, |h| h + 1),
			pos_count
		);
		*self.delta_line_hunks.borrow_mut() = hunk_map;
		*self.delta_line_positions.borrow_mut() = pos_map;
	}

	/// Parse old and new line numbers from delta gutter spans.
	/// Delta non-side-by-side format: `old_lineno ⋮ new_lineno │ content`
	/// Returns (`old_lineno`, `new_lineno`) — either may be `None` if empty.
	fn parse_delta_line_numbers(
		line: &Line<'_>,
	) -> (Option<u32>, Option<u32>) {
		let spans = &line.spans;
		// Need at least: old_lineno, ⋮, new_lineno, │
		if spans.len() < 4 {
			return (None, None);
		}
		// First span: old line number (may be empty for add lines)
		let old_num_text = spans[0].content.trim();
		// Second span: ⋮ separator
		let sep = spans[1].content.trim();
		if sep != "\u{22ee}" {
			return (None, None);
		}
		// Third span: new line number (may be empty for delete lines)
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
	fn refresh_delta_if_width_changed(&self) {
		if !self.is_delta_preview() {
			return;
		}
		let current_width = self.current_size.get().0;
		if current_width == self.last_delta_width.get()
			|| current_width == 0
			|| self.current.path.is_empty()
		{
			return;
		}
		let output = Self::run_delta_subprocess(
			&self.repo,
			&self.current.path,
			&self.current.diff_type,
			current_width,
			self.diff_mode == DiffMode::DeltaSideBySide,
		);
		*self.delta_output.borrow_mut() = output;
		self.rebuild_delta_maps();
		self.last_delta_width.set(current_width);
		// Update longest_line from new delta output
		self.longest_line.set(
			self.delta_output.borrow().as_ref().map_or(0, |lines| {
				lines
					.iter()
					.map(|line| {
						line.spans
							.iter()
							.map(|s| s.content.chars().count())
							.sum::<usize>()
					})
					.max()
					.unwrap_or(0)
			}),
		);
	}

	/// Cycle: `Unified` → `SideBySide` → `Delta` → `DeltaSideBySide` → `Unified`
	pub fn toggle_diff_mode(&mut self) {
		self.diff_mode = match self.diff_mode {
			DiffMode::Unified => DiffMode::SideBySide,
			DiffMode::SideBySide => DiffMode::Delta,
			DiffMode::Delta => DiffMode::DeltaSideBySide,
			DiffMode::DeltaSideBySide => DiffMode::Unified,
		};

		if self.is_delta_preview() && !Self::is_delta_available() {
			self.diff_mode = DiffMode::Unified;
			self.queue.push(InternalEvent::ShowErrorMsg(
				"delta not found. Install delta for enhanced diff preview."
					.to_string(),
			));
		}

		self.options.borrow_mut().set_diff_mode(self.diff_mode);

		if self.is_delta_preview() {
			self.run_delta();
		} else {
			*self.delta_output.borrow_mut() = None;
			self.delta_line_hunks.borrow_mut().clear();
			self.delta_line_positions.borrow_mut().clear();
		}
	}

	/// Calculate the line number width needed for side-by-side mode
	fn get_line_num_width(&self) -> usize {
		let Some(diff) = &self.diff else {
			return 1;
		};

		let max_line_num = diff
			.hunks
			.iter()
			.flat_map(|hunk| hunk.lines.iter())
			.flat_map(|line| {
				[line.position.old_lineno, line.position.new_lineno]
			})
			.flatten()
			.max()
			.unwrap_or(0);

		if max_line_num == 0 {
			1
		} else {
			(max_line_num.ilog10() + 1) as usize
		}
	}

	#[allow(clippy::too_many_lines)]
	fn get_side_by_side_lines(
		&self,
		height: u16,
	) -> Vec<SideBySideLine> {
		let Some(diff) = &self.diff else {
			return Vec::new();
		};

		if diff.hunks.is_empty() {
			return Vec::new();
		}

		let min = self.vertical_scroll.get_top();
		let max = min + height as usize;

		let mut result = Vec::new();
		// Use display_cursor to track display line index (where Delete+Add pairs count as 1)
		let mut display_cursor = 0_usize;

		for (hunk_idx, hunk) in diff.hunks.iter().enumerate() {
			// Calculate display line range for this hunk
			let hunk_display_start = display_cursor;
			let mut hunk_display_len = 0_usize;
			{
				let mut j = 0;
				while j < hunk.lines.len() {
					let line = &hunk.lines[j];
					if line.line_type == DiffLineType::Delete {
						if let Some(next) = hunk.lines.get(j + 1) {
							if next.line_type == DiffLineType::Add {
								j += 1;
							}
						}
					}
					hunk_display_len += 1;
					j += 1;
				}
			}
			let hunk_display_end =
				hunk_display_start + hunk_display_len;

			if Self::hunk_visible(
				hunk_display_start,
				hunk_display_end,
				min,
				max,
			) {
				let mut i = 0;
				while i < hunk.lines.len() {
					let line = &hunk.lines[i];
					let global_display_idx = display_cursor;
					let is_hunk_start = i == 0;
					// Calculate if this is the last display line of the hunk
					let is_hunk_end = {
						let mut remaining = hunk.lines.len() - i;
						let next = hunk.lines.get(i + 1);
						if line.line_type == DiffLineType::Delete
							&& next.is_some_and(|n| {
								n.line_type == DiffLineType::Add
							}) {
							remaining -= 1;
						}
						remaining == 1
					};

					if global_display_idx >= min
						&& global_display_idx <= max
					{
						match line.line_type {
							DiffLineType::Delete => {
								// Look ahead for a matching add line
								let next_line = hunk.lines.get(i + 1);
								let (
									right_content,
									right_num,
									right_type,
								) = next_line.map_or_else(
									|| {
										(
											String::new(),
											None,
											DiffLineType::None,
										)
									},
									|next| {
										if next.line_type
											== DiffLineType::Add
										{
											i += 1;
											(
												tabs_to_spaces(
													next.content
														.as_ref()
														.to_string(),
												),
												next.position
													.new_lineno,
												DiffLineType::Add,
											)
										} else {
											(
												String::new(),
												None,
												DiffLineType::None,
											)
										}
									},
								);

								result.push(SideBySideLine {
									left_content: tabs_to_spaces(
										line.content
											.as_ref()
											.to_string(),
									),
									left_line_num: line
										.position
										.old_lineno,
									right_content,
									right_line_num: right_num,
									left_type: DiffLineType::Delete,
									right_type,
									global_line_idx:
										global_display_idx,
									hunk_idx,
									is_hunk_start,
									is_hunk_end,
								});
							}
							DiffLineType::Add => {
								// Add line not paired with a delete
								result.push(SideBySideLine {
									left_content: String::new(),
									left_line_num: None,
									right_content: tabs_to_spaces(
										line.content
											.as_ref()
											.to_string(),
									),
									right_line_num: line
										.position
										.new_lineno,
									left_type: DiffLineType::None,
									right_type: DiffLineType::Add,
									global_line_idx:
										global_display_idx,
									hunk_idx,
									is_hunk_start,
									is_hunk_end,
								});
							}
							DiffLineType::Header => {
								let header_content = tabs_to_spaces(
									line.content.as_ref().to_string(),
								);
								result.push(SideBySideLine {
									left_content: header_content,
									left_line_num: None,
									right_content: String::new(),
									right_line_num: None,
									left_type: DiffLineType::Header,
									right_type: DiffLineType::Header,
									global_line_idx:
										global_display_idx,
									hunk_idx,
									is_hunk_start,
									is_hunk_end,
								});
							}
							DiffLineType::None => {
								// Context line - appears in both columns
								result.push(SideBySideLine {
									left_content: tabs_to_spaces(
										line.content
											.as_ref()
											.to_string(),
									),
									left_line_num: line
										.position
										.old_lineno,
									right_content: tabs_to_spaces(
										line.content
											.as_ref()
											.to_string(),
									),
									right_line_num: line
										.position
										.new_lineno,
									left_type: DiffLineType::None,
									right_type: DiffLineType::None,
									global_line_idx:
										global_display_idx,
									hunk_idx,
									is_hunk_start,
									is_hunk_end,
								});
							}
						}
					}

					// Increment display cursor for each display line
					display_cursor += 1;
					i += 1;
				}
			} else {
				// Skip this hunk's display lines
				display_cursor += hunk_display_len;
			}
		}

		result
	}

	/// Trim a `Line` from the left by `offset` characters, preserving styles.
	fn trim_line_offset(
		line: Line<'static>,
		offset: usize,
	) -> Line<'static> {
		if offset == 0 {
			return line;
		}
		let mut remaining = offset;
		let mut new_spans: Vec<Span<'static>> = Vec::new();
		for span in line.spans {
			let char_count = span.content.chars().count();
			if remaining >= char_count {
				// Skip this span entirely
				remaining -= char_count;
			} else {
				// Partially or fully include this span
				let trimmed: String =
					span.content.chars().skip(remaining).collect();
				remaining = 0;
				if !trimmed.is_empty() {
					new_spans.push(Span::styled(
						Cow::Owned(trimmed),
						span.style,
					));
				}
			}
		}
		Line::from(new_spans)
	}

	/// Pad a line's last span with spaces if it has a background color,
	/// so the background extends to the full panel width.
	fn pad_line_bg(
		mut line: Line<'static>,
		width: usize,
	) -> Line<'static> {
		let content_width: usize = line
			.spans
			.iter()
			.map(|s| s.content.chars().count())
			.sum();
		if content_width >= width {
			return line;
		}
		// Check if the last span has a background color
		if let Some(last) = line.spans.last() {
			if last.style.bg.is_some() {
				let pad = width - content_width;
				// Append spaces to the last span
				let mut new_content =
					String::with_capacity(last.content.len() + pad);
				new_content.push_str(&last.content);
				for _ in 0..pad {
					new_content.push(' ');
				}
				let idx = line.spans.len() - 1;
				line.spans[idx] =
					Span::styled(Cow::Owned(new_content), last.style);
			}
		}
		line
	}

	fn draw_delta(
		&self,
		f: &mut Frame,
		r: Rect,
		title: &str,
		height: u16,
	) {
		let delta = self.delta_output.borrow();
		let panel_width = usize::from(self.current_size.get().0);
		let scroll = self.vertical_scroll.get_top();
		let scrolled_right = self.horizontal_scroll.get_right();
		let cursor = self.selection.get_end().saturating_sub(scroll);
		let sel_style = self.theme.text(true, true);
		let txt: Vec<Line<'static>> = delta.as_ref().map_or_else(
			|| {
				vec![Line::from(vec![Span::styled(
					Cow::from("No delta output available."),
					self.theme.text(false, false),
				)])]
			},
			|lines| {
				lines
					.iter()
					.skip(scroll)
					.take(usize::from(height))
					.enumerate()
					.map(|(i, line)| {
						let mut line = Self::trim_line_offset(
							line.clone(),
							scrolled_right,
						);
						line = Self::pad_line_bg(line, panel_width);
						if i == cursor {
							for span in &mut line.spans {
								span.style = sel_style;
							}
							// Pad to full width with selection style
							let content_width: usize = line
								.spans
								.iter()
								.map(|s| s.content.chars().count())
								.sum();
							if content_width < panel_width {
								let padding = " ".repeat(
									panel_width - content_width,
								);
								line.spans.push(Span::styled(
									Cow::Owned(padding),
									sel_style,
								));
							}
						}
						line
					})
					.collect()
			},
		);

		f.render_widget(
			Paragraph::new(txt).block(
				Block::default()
					.title(Span::styled(
						title,
						self.theme.title(self.focused()),
					))
					.borders(Borders::ALL)
					.border_style(self.theme.block(self.focused())),
			),
			r,
		);

		if self.focused() {
			self.vertical_scroll.draw(f, r, &self.theme);

			if self.diff_mode != DiffMode::DeltaSideBySide
				&& self.max_scroll_right() > 0
			{
				self.horizontal_scroll.draw(f, r, &self.theme);
			}
		}
	}

	#[allow(clippy::too_many_lines)]
	#[allow(clippy::unnecessary_wraps)]
	fn draw_side_by_side(
		&self,
		f: &mut Frame,
		r: Rect,
		title: &str,
		height: u16,
		hunk_indicator: &str,
	) -> Result<()> {
		// First, get lines to calculate line number width
		let lines = self.get_side_by_side_lines(height);
		let line_num_width = self.get_line_num_width();

		// Split area into left and right columns
		let chunks = Layout::default()
			.direction(RatatuiDirection::Horizontal)
			.constraints(
				[
					Constraint::Percentage(50),
					Constraint::Percentage(50),
				]
				.as_ref(),
			)
			.split(r);

		// Calculate available width for content (subtract borders, marker, line number, space)
		// Each panel has: 1 border + 1 marker + line_num_width + 1 space chars overhead
		let panel_width = chunks[0].width.saturating_sub(
			2 + 1
				+ u16::try_from(line_num_width).unwrap_or(u16::MAX)
				+ 1,
		) as usize;
		let scrolled_right = self.horizontal_scroll.get_right();
		let selected_hunk = self.selected_hunk;

		// Get current selection index
		let current_selection = self.selection.get_end();

		// Build left column text with selection highlighting
		let left_txt: Vec<Line> = lines
			.iter()
			.map(|line| {
				let selected = self.focused()
					&& line.global_line_idx == current_selection;
				let hunk_selected = self.focused()
					&& selected_hunk
						.is_some_and(|h| h == line.hunk_idx);
				let left_content =
					trim_offset(&line.left_content, scrolled_right);
				let line_num_str = line.left_line_num.map_or_else(
					|| " ".repeat(line_num_width),
					|n| format!("{n:line_num_width$}"),
				);

				// Get hunk marker style
				let marker_style =
					self.theme.diff_hunk_marker(hunk_selected);
				let marker = if line.is_hunk_end {
					symbols::line::BOTTOM_LEFT
				} else if line.is_hunk_start {
					symbols::line::TOP_LEFT
				} else {
					symbols::line::VERTICAL
				};

				// Pad content to fill width when selected
				let content = if selected {
					format!("{left_content:panel_width$}\n")
				} else {
					format!("{left_content}\n")
				};

				// For lines where left side is empty (e.g., Add lines without Delete pair),
				// still apply selection highlight to maintain visual consistency.
				// Show line_break symbol (¶) for empty Add/Delete lines, same as unified mode.
				if line.left_content.is_empty() {
					// Show line_break symbol for empty Add/Delete lines
					let display_content =
						if line.left_type == DiffLineType::None {
							String::new()
						} else {
							self.theme.line_break()
						};
					let content = if selected {
						format!("{display_content:panel_width$}\n")
					} else {
						format!("{display_content}\n")
					};
					Line::from(vec![
						Span::styled(Cow::from(marker), marker_style),
						Span::styled(
							Cow::from(line_num_str),
							self.theme.text(false, false),
						),
						Span::styled(
							Cow::from(" "),
							self.theme.text(false, false),
						),
						Span::styled(
							Cow::from(content),
							self.theme
								.diff_line(line.left_type, selected),
						),
					])
				} else {
					Line::from(vec![
						Span::styled(Cow::from(marker), marker_style),
						Span::styled(
							Cow::from(line_num_str),
							self.theme.text(false, false),
						),
						// Gap between line number and content - never highlighted
						Span::styled(
							Cow::from(" "),
							self.theme.text(false, false),
						),
						Span::styled(
							Cow::from(content),
							self.theme
								.diff_line(line.left_type, selected),
						),
					])
				}
			})
			.collect();

		// Build right column text with selection highlighting
		let right_txt: Vec<Line> = lines
			.iter()
			.map(|line| {
				let selected = self.focused()
					&& line.global_line_idx == current_selection;
				let hunk_selected = self.focused()
					&& selected_hunk
						.is_some_and(|h| h == line.hunk_idx);
				let right_content =
					trim_offset(&line.right_content, scrolled_right);
				let line_num_str = line.right_line_num.map_or_else(
					|| " ".repeat(line_num_width),
					|n| format!("{n:line_num_width$}"),
				);

				// Get hunk marker style
				let marker_style =
					self.theme.diff_hunk_marker(hunk_selected);
				let marker = if line.is_hunk_end {
					symbols::line::BOTTOM_LEFT
				} else if line.is_hunk_start {
					symbols::line::TOP_LEFT
				} else {
					symbols::line::VERTICAL
				};

				// Pad content to fill width when selected
				let content = if selected {
					format!("{right_content:panel_width$}\n")
				} else {
					format!("{right_content}\n")
				};

				// For lines where right side is empty (Header or paired Delete),
				// still apply selection highlight to maintain visual consistency.
				// Show line_break symbol (¶) for empty Add/Delete lines, same as unified mode.
				if line.right_type == DiffLineType::Header
					|| line.right_content.is_empty()
				{
					// Show line_break symbol for empty Add/Delete lines (but not Header)
					let display_content = if line.right_type
						!= DiffLineType::None
						&& line.right_type != DiffLineType::Header
					{
						self.theme.line_break()
					} else {
						String::new()
					};
					let filler = if selected {
						format!("{display_content:panel_width$}\n")
					} else {
						format!("{display_content}\n")
					};
					Line::from(vec![
						Span::styled(Cow::from(marker), marker_style),
						Span::styled(
							Cow::from(line_num_str),
							self.theme.text(false, false),
						),
						Span::styled(
							Cow::from(" "),
							self.theme.text(false, false),
						),
						Span::styled(
							Cow::from(filler),
							self.theme
								.diff_line(line.right_type, selected),
						),
					])
				} else {
					Line::from(vec![
						Span::styled(Cow::from(marker), marker_style),
						Span::styled(
							Cow::from(line_num_str),
							self.theme.text(false, false),
						),
						// Gap between line number and content - never highlighted
						Span::styled(
							Cow::from(" "),
							self.theme.text(false, false),
						),
						Span::styled(
							Cow::from(content),
							self.theme
								.diff_line(line.right_type, selected),
						),
					])
				}
			})
			.collect();

		// Draw left column
		f.render_widget(
			Paragraph::new(left_txt).block(
				Block::default()
					.title(Span::styled(
						format!("{title} [Old]{hunk_indicator}"),
						self.theme.title(self.focused()),
					))
					.borders(Borders::ALL)
					.border_style(self.theme.block(self.focused())),
			),
			chunks[0],
		);

		// Draw right column
		f.render_widget(
			Paragraph::new(right_txt).block(
				Block::default()
					.title(Span::styled(
						format!("[New]{hunk_indicator}"),
						self.theme.title(self.focused()),
					))
					.borders(Borders::ALL)
					.border_style(self.theme.block(self.focused())),
			),
			chunks[1],
		);

		if self.focused() {
			self.vertical_scroll.draw(f, r, &self.theme);

			if self.max_scroll_right() > 0 {
				self.horizontal_scroll.draw(f, r, &self.theme);
			}
		}

		Ok(())
	}

	const fn is_stage(&self) -> bool {
		self.current.is_stage
	}
}

impl DrawableComponent for DiffComponent {
	#[allow(clippy::too_many_lines)]
	fn draw(&self, f: &mut Frame, r: Rect) -> Result<()> {
		self.current_size.set((
			r.width.saturating_sub(2),
			r.height.saturating_sub(2),
		));

		self.refresh_delta_if_width_changed();

		let current_width = self.current_size.get().0;
		let current_height = self.current_size.get().1;

		// Use display line count for side-by-side mode
		let lines_count = if self.diff_mode == DiffMode::SideBySide {
			self.side_by_side_lines_count()
		} else if self.is_delta_preview() {
			self.delta_output.borrow().as_ref().map_or(0, Vec::len)
		} else {
			self.lines_count()
		};

		self.vertical_scroll.update(
			self.selection.get_end(),
			lines_count,
			usize::from(current_height),
		);

		// Calculate content width for horizontal scroll
		// In side-by-side mode, each panel content width is smaller
		// chunks[0].width ≈ r.width / 2, content = chunks[0].width - (2 + 1 + line_num_width + 1)
		// ≈ current_width / 2 - (2 + 1 + line_num_width + 1)
		// In unified mode, we have two line number columns
		let line_num_width: u16 =
			self.get_line_num_width().try_into().unwrap_or(u16::MAX);
		let panel_content_width: usize =
			if self.diff_mode == DiffMode::SideBySide {
				(current_width / 2)
					.saturating_sub(2 + 1 + line_num_width + 1)
					.into()
			} else if self.is_delta_preview() {
				usize::from(current_width)
			} else {
				// In unified mode with line numbers
				let line_num_overhead = line_num_width * 2 + 3;
				current_width.saturating_sub(line_num_overhead).into()
			};

		self.horizontal_scroll.update_no_selection(
			self.longest_line.get(),
			panel_content_width,
		);

		let hunk_info =
			self.diff.as_ref().map_or_else(String::new, |diff| {
				if diff.hunks.is_empty() {
					return String::new();
				}
				self.selected_hunk.map_or_else(
					String::new,
					|selected| {
						format!(
							" [{}/{}]",
							selected + 1,
							diff.hunks.len()
						)
					},
				)
			});

		// For side-by-side mode, hunk indicator will be added to [Old] and [New] titles
		let title = format!(
			"{}{}{}",
			strings::title_diff(&self.key_config),
			self.current.path,
			if self.diff_mode == DiffMode::SideBySide {
				""
			} else {
				&hunk_info
			}
		);

		if self.diff_mode == DiffMode::SideBySide && !self.pending {
			self.draw_side_by_side(
				f,
				r,
				&title,
				current_height,
				&hunk_info,
			)?;
		} else if self.is_delta_preview() && !self.pending {
			self.draw_delta(f, r, &title, current_height);
		} else {
			let txt = if self.pending {
				vec![Line::from(vec![Span::styled(
					Cow::from(strings::loading_text(
						&self.key_config,
					)),
					self.theme.text(false, false),
				)])]
			} else {
				self.get_text(r.width, current_height)
			};

			f.render_widget(
				Paragraph::new(txt).block(
					Block::default()
						.title(Span::styled(
							title.as_str(),
							self.theme.title(self.focused()),
						))
						.borders(Borders::ALL)
						.border_style(
							self.theme.block(self.focused()),
						),
				),
				r,
			);

			if self.focused() {
				self.vertical_scroll.draw(f, r, &self.theme);

				if self.max_scroll_right() > 0 {
					self.horizontal_scroll.draw(f, r, &self.theme);
				}
			}
		}

		Ok(())
	}
}

impl Component for DiffComponent {
	fn commands(
		&self,
		out: &mut Vec<CommandInfo>,
		_force_all: bool,
	) -> CommandBlocking {
		out.push(CommandInfo::new(
			strings::commands::scroll(&self.key_config),
			self.can_scroll(),
			self.focused(),
		));
		out.push(CommandInfo::new(
			strings::commands::diff_hunk_next(&self.key_config),
			self.calc_hunk_move_target(1) != self.selected_hunk,
			self.focused(),
		));
		out.push(CommandInfo::new(
			strings::commands::diff_hunk_prev(&self.key_config),
			self.calc_hunk_move_target(-1) != self.selected_hunk,
			self.focused(),
		));
		out.push(
			CommandInfo::new(
				strings::commands::diff_home_end(&self.key_config),
				self.can_scroll(),
				self.focused(),
			)
			.hidden(),
		);

		if !self.is_immutable {
			// Hunk-level stage/unstage — works in all modes including delta
			out.push(CommandInfo::new(
				strings::commands::diff_hunk_remove(&self.key_config),
				self.selected_hunk.is_some(),
				self.focused() && self.is_stage(),
			));
			out.push(CommandInfo::new(
				strings::commands::diff_hunk_add(&self.key_config),
				self.selected_hunk.is_some(),
				self.focused() && !self.is_stage(),
			));
			out.push(CommandInfo::new(
				strings::commands::diff_hunk_revert(&self.key_config),
				self.selected_hunk.is_some(),
				self.focused() && !self.is_stage(),
			));
			// Line-level stage/unstage
			if !self.is_delta_preview() {
				out.push(CommandInfo::new(
					strings::commands::diff_lines_revert(
						&self.key_config,
					),
					true,
					self.focused() && !self.is_stage(),
				));
			}
			out.push(CommandInfo::new(
				strings::commands::diff_lines_stage(&self.key_config),
				true,
				self.focused() && !self.is_stage(),
			));
			out.push(CommandInfo::new(
				strings::commands::diff_lines_unstage(
					&self.key_config,
				),
				true,
				self.focused() && self.is_stage(),
			));
		}

		out.push(CommandInfo::new(
			strings::commands::copy(&self.key_config),
			true,
			self.focused(),
		));
		out.push(CommandInfo::new(
			strings::commands::diff_toggle_mode(&self.key_config),
			true,
			self.focused(),
		));

		CommandBlocking::PassingOn
	}

	#[allow(clippy::cognitive_complexity, clippy::too_many_lines)]
	fn event(&mut self, ev: &Event) -> Result<EventState> {
		if self.focused() {
			if let Event::Key(e) = ev {
				return if key_match(e, self.key_config.keys.move_down)
				{
					self.move_selection(ScrollType::Down);
					Ok(EventState::Consumed)
				} else if key_match(
					e,
					self.key_config.keys.shift_down,
				) {
					self.modify_selection(Direction::Down);
					Ok(EventState::Consumed)
				} else if key_match(e, self.key_config.keys.shift_up)
				{
					self.modify_selection(Direction::Up);
					Ok(EventState::Consumed)
				} else if key_match(e, self.key_config.keys.end) {
					self.move_selection(ScrollType::End);
					Ok(EventState::Consumed)
				} else if key_match(e, self.key_config.keys.home) {
					self.move_selection(ScrollType::Home);
					Ok(EventState::Consumed)
				} else if key_match(e, self.key_config.keys.move_up) {
					self.move_selection(ScrollType::Up);
					Ok(EventState::Consumed)
				} else if key_match(e, self.key_config.keys.page_up) {
					self.move_selection(ScrollType::PageUp);
					Ok(EventState::Consumed)
				} else if key_match(e, self.key_config.keys.page_down)
				{
					self.move_selection(ScrollType::PageDown);
					Ok(EventState::Consumed)
				} else if key_match(
					e,
					self.key_config.keys.move_right,
				) {
					self.horizontal_scroll
						.move_right(HorizontalScrollType::Right);
					Ok(EventState::Consumed)
				} else if key_match(e, self.key_config.keys.move_left)
				{
					self.horizontal_scroll
						.move_right(HorizontalScrollType::Left);
					Ok(EventState::Consumed)
				} else if key_match(
					e,
					self.key_config.keys.diff_line_start,
				) || key_match(
					e,
					GituiKeyEvent::new(
						KeyCode::Char('_'),
						KeyModifiers::empty(),
					),
				) {
					self.horizontal_scroll
						.move_right(HorizontalScrollType::Home);
					Ok(EventState::Consumed)
				} else if key_match(
					e,
					self.key_config.keys.diff_line_end,
				) {
					self.horizontal_scroll
						.move_right(HorizontalScrollType::End);
					Ok(EventState::Consumed)
				} else if key_match(
					e,
					self.key_config.keys.diff_hunk_next,
				) {
					self.diff_hunk_move_up_down(1);
					Ok(EventState::Consumed)
				} else if key_match(
					e,
					self.key_config.keys.diff_hunk_prev,
				) {
					self.diff_hunk_move_up_down(-1);
					Ok(EventState::Consumed)
				} else if key_match(
					e,
					self.key_config.keys.stage_unstage_item,
				) && !self.is_immutable
				{
					try_or_popup!(
						self,
						"hunk error:",
						self.stage_unstage_hunk()
					);

					Ok(EventState::Consumed)
				} else if key_match(
					e,
					self.key_config.keys.status_reset_item,
				) && !self.is_immutable
					&& !self.is_stage()
					&& !self.is_delta_preview()
				{
					if let Some(diff) = &self.diff {
						if diff.untracked {
							self.reset_untracked();
						} else {
							self.reset_hunk();
						}
					}
					Ok(EventState::Consumed)
				} else if key_match(
					e,
					self.key_config.keys.diff_stage_lines,
				) && !self.is_immutable
				{
					self.stage_lines();
					Ok(EventState::Consumed)
				} else if key_match(
					e,
					self.key_config.keys.diff_reset_lines,
				) && !self.is_immutable
					&& !self.is_stage()
					&& !self.is_delta_preview()
				{
					if let Some(diff) = &self.diff {
						//TODO: reset untracked lines
						if !diff.untracked {
							self.reset_lines();
						}
					}
					Ok(EventState::Consumed)
				} else if key_match(e, self.key_config.keys.copy) {
					self.copy_selection();
					Ok(EventState::Consumed)
				} else if key_match(
					e,
					self.key_config.keys.diff_mode_toggle,
				) {
					self.toggle_diff_mode();
					Ok(EventState::Consumed)
				} else {
					Ok(EventState::NotConsumed)
				};
			}
		}

		Ok(EventState::NotConsumed)
	}

	fn focused(&self) -> bool {
		self.focused
	}
	fn focus(&mut self, focus: bool) {
		self.focused = focus;
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::ui::style::Theme;
	use std::io::Write;
	use std::rc::Rc;
	use tempfile::NamedTempFile;

	#[test]
	fn test_line_break() {
		let diff_line = DiffLine {
			content: "".into(),
			line_type: DiffLineType::Add,
			position: Default::default(),
		};

		{
			let default_theme = Rc::new(Theme::default());

			assert_eq!(
				DiffComponent::get_line_to_add(
					4,
					&diff_line,
					false,
					false,
					false,
					&default_theme,
					0,
					1
				)
				.spans
				.last()
				.unwrap(),
				&Span::styled(
					Cow::from("¶\n"),
					default_theme
						.diff_line(diff_line.line_type, false)
				)
			);
		}

		{
			let mut file = NamedTempFile::new().unwrap();

			writeln!(
				file,
				r#"
(
	line_break: Some("+")
)
"#
			)
			.unwrap();

			let theme =
				Rc::new(Theme::init(&file.path().to_path_buf()));

			assert_eq!(
				DiffComponent::get_line_to_add(
					4, &diff_line, false, false, false, &theme, 0, 1
				)
				.spans
				.last()
				.unwrap(),
				&Span::styled(
					Cow::from("+\n"),
					theme.diff_line(diff_line.line_type, false)
				)
			);
		}
	}
}
