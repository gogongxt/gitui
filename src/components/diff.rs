use super::{
	utils::scroll_horizontal::HorizontalScroll,
	utils::scroll_vertical::VerticalScroll, CommandBlocking,
	Direction, DrawableComponent, HorizontalScrollType, ScrollType,
};
use crate::{
	app::Environment,
	components::{
		async_delta::{
			pad_line_bg as delta_pad_line_bg, AsyncDelta,
			DeltaParams, ProcessedDelta,
		},
		CommandInfo, Component, EventState,
	},
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
	style::Color,
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
	delta_line_level_bgs: RefCell<Vec<Option<Color>>>,
	delta_line_hunks: RefCell<Vec<usize>>,
	delta_line_positions: RefCell<Vec<Option<DiffLinePosition>>>,
	last_delta_width: Cell<u16>,
	delta_display_lines: RefCell<Vec<Line<'static>>>,
	async_delta: AsyncDelta,
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
			delta_line_level_bgs: RefCell::new(Vec::new()),
			delta_line_hunks: RefCell::new(Vec::new()),
			delta_line_positions: RefCell::new(Vec::new()),
			last_delta_width: Cell::new(0),
			delta_display_lines: RefCell::new(Vec::new()),
			async_delta: AsyncDelta::new(&env.sender_git),
		}
	}
	///
	fn can_scroll(&self) -> bool {
		if self.is_delta_preview() {
			return self.delta_display_lines.borrow().len() > 1;
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
		self.delta_line_level_bgs.borrow_mut().clear();
		self.delta_line_hunks.borrow_mut().clear();
		self.delta_line_positions.borrow_mut().clear();
		self.delta_display_lines.borrow_mut().clear();
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
					// File switch: async render is fine (cursor resets anyway)
					self.vertical_scroll.reset();
					self.selection = Selection::Single(0);
					self.request_delta();
				} else {
					// Same file, content changed (stage/unstage): render
					// synchronously to preserve cursor and avoid flicker.
					self.request_delta_sync();
				}
				// Clamp selection to new display line count
				let max = self
					.delta_display_lines
					.borrow()
					.len()
					.saturating_sub(1);
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
		// In delta mode, scroll based on display lines
		if self.is_delta_preview() {
			let max = self
				.delta_display_lines
				.borrow()
				.len()
				.saturating_sub(1);
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
			return self.delta_display_lines.borrow().len();
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
			let selected_lines = self.selected_lines();

			if diff.untracked {
				try_or_popup!(
					self,
					"(un)stage lines:",
					sync::stage_lines_untracked(
						&self.repo.borrow(),
						&self.current.path,
						&selected_lines,
					)
				);
			} else {
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
			}

			self.queue_update();
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

	/// Request a delta render. On cache hit, applies the result
	/// synchronously. On miss, spawns a background job; when it
	/// finishes, the tab calls `apply_delta()` to apply the result.
	fn request_delta(&self) {
		let params = DeltaParams {
			path: self.current.path.clone(),
			diff_type: self.current.diff_type.clone(),
			width: self.current_size.get().0,
			side_by_side: self.diff_mode == DiffMode::DeltaSideBySide,
			diff_hash: self.current.hash,
		};
		let repo = self.repo.borrow().clone();
		let diff = self.diff.clone();
		if let Some(result) =
			self.async_delta.request(params, repo, diff)
		{
			self.apply_delta_result(result);
			self.last_delta_width.set(self.current_size.get().0);
		}
		// On cache miss, keep showing the previous delta output while
		// the background job runs. Clearing `delta_display_lines`
		// here would flash "Loading..." and reset scroll on every
		// stage/unstage, since `apply_delta` only clamps selection
		// after the new result arrives.
	}

	/// Synchronous delta render for same-file content changes
	/// (stage/unstage hunk, stage/unstage lines). Runs on the current
	/// thread to preserve cursor and avoid flicker — the async path
	/// loses scroll position because `apply_delta` runs in a separate
	/// event tick after the background job finishes.
	///
	/// File switches still use `request_delta()` (async).
	fn request_delta_sync(&self) {
		let params = DeltaParams {
			path: self.current.path.clone(),
			diff_type: self.current.diff_type.clone(),
			width: self.current_size.get().0,
			side_by_side: self.diff_mode == DiffMode::DeltaSideBySide,
			diff_hash: self.current.hash,
		};
		let repo = self.repo.borrow();
		let diff = self.diff.as_ref();
		if let Some(result) =
			self.async_delta.request_sync(&params, &repo, diff)
		{
			self.apply_delta_result(result);
			self.last_delta_width.set(self.current_size.get().0);
		}
	}

	/// Apply a processed delta result (from cache or background job).
	/// Pure assignment — all heavy lifting was done on the worker thread.
	fn apply_delta_result(&self, result: ProcessedDelta) {
		let ProcessedDelta {
			display_lines,
			display_hunks,
			display_positions,
			line_level_bgs,
		} = result;
		*self.delta_line_level_bgs.borrow_mut() = line_level_bgs;
		*self.delta_line_hunks.borrow_mut() = display_hunks;
		*self.delta_line_positions.borrow_mut() = display_positions;
		*self.delta_display_lines.borrow_mut() = display_lines;
	}

	/// Called by the tab when `AsyncGitNotification::Delta` arrives.
	/// Applies the pending result if it matches the current params.
	pub fn apply_delta(&mut self) {
		let params = DeltaParams {
			path: self.current.path.clone(),
			diff_type: self.current.diff_type.clone(),
			width: self.current_size.get().0,
			side_by_side: self.diff_mode == DiffMode::DeltaSideBySide,
			diff_hash: self.current.hash,
		};
		if let Some(result) =
			self.async_delta.take_if_matches(&params)
		{
			self.apply_delta_result(result);
			self.last_delta_width.set(self.current_size.get().0);
			// Clamp selection to new display line count
			let max = self
				.delta_display_lines
				.borrow()
				.len()
				.saturating_sub(1);
			if let Selection::Single(line) = &self.selection {
				if *line > max {
					self.selection = Selection::Single(max);
				}
			}
			let idx = self.selection.get_end();
			let hunk_map = self.delta_line_hunks.borrow();
			let max_hunk = self
				.diff
				.as_ref()
				.map_or(0, |d| d.hunks.len().saturating_sub(1));
			self.selected_hunk =
				hunk_map.get(idx).copied().map(|h| h.min(max_hunk));
		}
	}

	/// Returns true if a delta render is in-flight for the current
	/// params.
	pub fn is_delta_pending(&self) -> bool {
		self.async_delta.is_pending()
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
		self.request_delta();
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
			self.request_delta();
		} else {
			self.delta_line_level_bgs.borrow_mut().clear();
			self.delta_line_hunks.borrow_mut().clear();
			self.delta_line_positions.borrow_mut().clear();
			self.delta_display_lines.borrow_mut().clear();
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

	fn draw_delta(
		&self,
		f: &mut Frame,
		r: Rect,
		title: &str,
		height: u16,
	) {
		let display = self.delta_display_lines.borrow();
		let panel_width = usize::from(self.current_size.get().0);
		let scroll = self.vertical_scroll.get_top();
		let cursor = self.selection.get_end().saturating_sub(scroll);
		let sel_style = self.theme.text(true, true);
		let txt: Vec<Line<'static>> = if display.is_empty() {
			vec![Line::from(vec![Span::styled(
				Cow::from("No delta output available."),
				self.theme.text(false, false),
			)])]
		} else {
			display
				.iter()
				.skip(scroll)
				.take(usize::from(height))
				.enumerate()
				.map(|(i, line)| {
					let mut line = {
						let is_sbs = self.diff_mode
							== DiffMode::DeltaSideBySide;
						if is_sbs {
							delta_pad_line_bg(
								line.clone(),
								panel_width,
								true,
								None,
							)
						} else {
							// Already padded by rebuild_display_lines with dominant bg
							line.clone()
						}
					};
					if i == cursor {
						for span in &mut line.spans {
							span.style = sel_style;
						}
						// Pad to full width with selection style
						let content_width: usize = line
							.spans
							.iter()
							.map(|s| {
								unicode_width::UnicodeWidthStr::width(
									s.content.as_ref(),
								)
							})
							.sum();
						if content_width < panel_width {
							let padding = " "
								.repeat(panel_width - content_width);
							line.spans.push(Span::styled(
								Cow::Owned(padding),
								sel_style,
							));
						}
					}
					line
				})
				.collect()
		};

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
			self.delta_display_lines.borrow().len()
		} else {
			self.lines_count()
		};

		self.vertical_scroll.update(
			self.selection.get_end(),
			lines_count,
			usize::from(current_height),
		);

		// Calculate content width for horizontal scroll (non-delta modes only)
		if !self.is_delta_preview() {
			let line_num_width: u16 = self
				.get_line_num_width()
				.try_into()
				.unwrap_or(u16::MAX);
			let panel_content_width: usize = if self.diff_mode
				== DiffMode::SideBySide
			{
				(current_width / 2)
					.saturating_sub(2 + 1 + line_num_width + 1)
					.into()
			} else {
				// In unified mode with line numbers
				let line_num_overhead = line_num_width * 2 + 3;
				current_width.saturating_sub(line_num_overhead).into()
			};
			self.horizontal_scroll.update_no_selection(
				self.longest_line.get(),
				panel_content_width,
			);
		}

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

		// Show "Loading..." only when we have no delta output at all
		// (first render for this file). While a background re-render is
		// in flight, keep showing the previous output to avoid flicker
		// and cursor loss.
		let delta_pending = self.is_delta_preview()
			&& self.is_delta_pending()
			&& self.delta_display_lines.borrow().is_empty();

		if self.diff_mode == DiffMode::SideBySide && !self.pending {
			self.draw_side_by_side(
				f,
				r,
				&title,
				current_height,
				&hunk_info,
			)?;
		} else if self.is_delta_preview()
			&& !self.pending
			&& !delta_pending
		{
			self.draw_delta(f, r, &title, current_height);
		} else {
			let txt = if self.pending || delta_pending {
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
					|| key_match(e, self.key_config.keys.popup_down)
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
				} else if key_match(e, self.key_config.keys.move_up)
					|| key_match(e, self.key_config.keys.popup_up)
				{
					self.move_selection(ScrollType::Up);
					Ok(EventState::Consumed)
				} else if key_match(e, self.key_config.keys.page_up) {
					self.move_selection(ScrollType::PageUp);
					Ok(EventState::Consumed)
				} else if key_match(e, self.key_config.keys.page_down)
				{
					self.move_selection(ScrollType::PageDown);
					Ok(EventState::Consumed)
				} else if !self.is_delta_preview()
					&& key_match(e, self.key_config.keys.move_right)
				{
					self.horizontal_scroll
						.move_right(HorizontalScrollType::Right);
					Ok(EventState::Consumed)
				} else if !self.is_delta_preview()
					&& key_match(e, self.key_config.keys.move_left)
				{
					self.horizontal_scroll
						.move_right(HorizontalScrollType::Left);
					Ok(EventState::Consumed)
				} else if !self.is_delta_preview()
					&& (key_match(
						e,
						self.key_config.keys.diff_line_start,
					) || key_match(
						e,
						GituiKeyEvent::new(
							KeyCode::Char('_'),
							KeyModifiers::empty(),
						),
					)) {
					self.horizontal_scroll
						.move_right(HorizontalScrollType::Home);
					Ok(EventState::Consumed)
				} else if !self.is_delta_preview()
					&& key_match(
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
	use crate::app::Environment;
	use crate::components::async_delta::wrap_line as delta_wrap_line;
	use crate::ui::style::Theme;
	use asyncgit::sync::RepoPath;
	use ratatui::style::{Color, Style};
	use std::io::Write;
	use std::rc::Rc;
	use tempfile::NamedTempFile;

	/// Build a `FileDiff` by running `git diff` against a real repo.
	fn make_filediff(
		repo: &RepoPath,
		path: &str,
		is_stage: bool,
	) -> FileDiff {
		asyncgit::sync::diff::get_diff(repo, path, is_stage, None)
			.expect("get_diff failed")
	}

	/// Reproduces: delta mode, same file, content changes after stage line.
	/// Cursor should be preserved (clamped to new max), NOT reset to 0.
	#[test]
	fn test_delta_preserves_cursor_on_content_change() {
		use tempfile::TempDir;
		if !DiffComponent::is_delta_available() {
			eprintln!("skipping: delta binary not on PATH");
			return;
		}
		let td = TempDir::new().unwrap();

		// Init git repo
		let mut cmd = std::process::Command::new("git");
		cmd.args(["init"]).current_dir(td.path());
		cmd.output().unwrap();
		let mut cmd = std::process::Command::new("git");
		cmd.args(["config", "user.email", "t@t.t"])
			.current_dir(td.path());
		cmd.output().unwrap();
		let mut cmd = std::process::Command::new("git");
		cmd.args(["config", "user.name", "t"])
			.current_dir(td.path());
		cmd.output().unwrap();

		// Commit initial file
		let file_path = td.path().join("f.txt");
		std::fs::write(
			&file_path,
			"line1\nline2\nline3\nline4\nline5\nline6\nline7\nline8\nline9\nline10\n",
		).unwrap();
		let mut cmd = std::process::Command::new("git");
		cmd.args(["add", "f.txt"]).current_dir(td.path());
		cmd.output().unwrap();
		let mut cmd = std::process::Command::new("git");
		cmd.args(["commit", "-m", "init"]).current_dir(td.path());
		cmd.output().unwrap();

		// Modify file: change line 2 and add lines at end
		std::fs::write(
			&file_path,
			"line1\nLINE2_CHANGED\nline3\nline4\nline5\nline6\nline7\nline8\nline9\nline10\nline11_NEW\nline12_NEW\n",
		).unwrap();

		let repo = RepoPath::Path(td.path().to_path_buf());

		// Build DiffComponent in delta mode
		let env = Environment::test_env();
		let mut diff_comp = DiffComponent::new(&env, false);
		// Point the component at our temp repo
		*diff_comp.repo.borrow_mut() = repo.clone();
		// Force delta mode: Unified -> SideBySide -> Delta
		diff_comp.toggle_diff_mode();
		diff_comp.toggle_diff_mode();
		assert!(diff_comp.is_delta_preview());
		diff_comp.current_size.set((120, 40));

		// First update: initial diff (workdir) — file switch path (async).
		// Simulate the async completion by calling request_sync directly
		// to populate the display lines, as the event loop isn't running.
		let diff1 = make_filediff(&repo, "f.txt", false);
		assert!(!diff1.hunks.is_empty(), "should have diff hunks");
		diff_comp.update(
			"f.txt".to_string(),
			false,
			diff1.clone(),
			DiffType::WorkDir,
		);
		// Simulate async delta completion by running sync
		let params1 = DeltaParams {
			path: "f.txt".to_string(),
			diff_type: DiffType::WorkDir,
			width: 120,
			side_by_side: false,
			diff_hash: diff_comp.current.hash,
		};
		if let Some(result) = diff_comp.async_delta.request_sync(
			&params1,
			&repo,
			Some(&diff1),
		) {
			diff_comp.apply_delta_result(result);
		}
		let display_len_after_first =
			diff_comp.delta_display_lines.borrow().len();
		assert!(
			display_len_after_first > 0,
			"should have delta output after first update, got {}",
			display_len_after_first
		);

		// Move cursor down 5 times
		for _ in 0..5 {
			diff_comp.move_selection(ScrollType::Down);
		}
		let sel_after_move = match diff_comp.selection {
			Selection::Single(n) => n,
			_ => panic!("expected Single"),
		};
		assert_eq!(
			sel_after_move, 5,
			"cursor should be at 5 after 5 downs"
		);

		// Stage all changes — workdir diff becomes empty, stage diff has content.
		let mut cmd = std::process::Command::new("git");
		cmd.args(["add", "f.txt"]).current_dir(td.path());
		cmd.output().unwrap();

		// Modify file again so workdir diff is non-empty but different.
		std::fs::write(
			&file_path,
			"line1\nLINE2_CHANGED\nline3\nline4\nline5\nline6\nline7\nline8\nline9\nline10\nline11_NEW\nline12_NEW\nline13_NEW2\n",
		).unwrap();

		let diff2 = make_filediff(&repo, "f.txt", false);
		assert!(
			!diff2.hunks.is_empty(),
			"should still have diff hunks"
		);

		// Call update with new diff — should preserve cursor
		let hash_before = diff_comp.current.hash;
		diff_comp.update(
			"f.txt".to_string(),
			false,
			diff2,
			DiffType::WorkDir,
		);
		let hash_after = diff_comp.current.hash;
		assert_ne!(
			hash_before, hash_after,
			"hash should have changed"
		);

		let sel_after_update = match diff_comp.selection {
			Selection::Single(n) => n,
			_ => panic!("expected Single"),
		};
		// Cursor should still be at 5 (or clamped to new max if max < 5)
		let display_len =
			diff_comp.delta_display_lines.borrow().len();
		let expected_max = display_len.saturating_sub(1);
		assert!(
			sel_after_update <= expected_max,
			"cursor {} should be <= max {}",
			sel_after_update,
			expected_max
		);
		assert!(
			sel_after_update > 0 || expected_max == 0,
			"cursor was reset to 0 but display has {} lines",
			display_len
		);
	}

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

	#[test]
	fn test_wrap_line_exact_width() {
		let line = Line::from(Span::raw("a".repeat(40)));
		let wrapped = delta_wrap_line(&line, 40);
		assert_eq!(
			wrapped.len(),
			1,
			"exact width should produce 1 line, got {}",
			wrapped.len()
		);
	}

	/// Reproduces the non-SBS delta preview truncation bug: a long
	/// added line that should wrap into N rows in a narrow preview
	/// must render all N rows. The wrapped line count (from
	/// `rebuild`) must match the number of rows the Paragraph
	/// actually renders inside the bordered rect.
	#[test]
	fn test_delta_preview_renders_all_wrapped_rows() {
		use ratatui::{backend::TestBackend, Terminal};
		use tempfile::TempDir;
		if !DiffComponent::is_delta_available() {
			eprintln!("skipping: delta binary not on PATH");
			return;
		}
		let td = TempDir::new().unwrap();
		for mut c in [
			std::process::Command::new("git"),
			std::process::Command::new("git"),
			std::process::Command::new("git"),
		] {
			let _ = c.current_dir(td.path()).output();
		}
		std::process::Command::new("git")
			.args(["init"])
			.current_dir(td.path())
			.output()
			.unwrap();
		std::process::Command::new("git")
			.args(["config", "user.email", "t@t.t"])
			.current_dir(td.path())
			.output()
			.unwrap();
		std::process::Command::new("git")
			.args(["config", "user.name", "t"])
			.current_dir(td.path())
			.output()
			.unwrap();

		let file_path = td.path().join("f.txt");
		std::fs::write(&file_path, "short\n").unwrap();
		std::process::Command::new("git")
			.args(["add", "f.txt"])
			.current_dir(td.path())
			.output()
			.unwrap();
		std::process::Command::new("git")
			.args(["commit", "-m", "init"])
			.current_dir(td.path())
			.output()
			.unwrap();

		// A long line of CJK characters. Each CJK char occupies 2
		// terminal cells, so 40 chars = 80 cells. At panel width 40,
		// this should wrap into 2 rows (80/40), but `wrap_line` uses
		// chars().count() (treating CJK as width 1), so it thinks the
		// line fits in 1 row — causing truncation.
		let long_line = "中".repeat(40);
		std::fs::write(&file_path, format!("short\n{long_line}\n"))
			.unwrap();

		let repo = RepoPath::Path(td.path().to_path_buf());

		let env = Environment::test_env();
		let mut diff_comp = DiffComponent::new(&env, false);
		*diff_comp.repo.borrow_mut() = repo.clone();
		// Force delta mode: Unified -> SideBySide -> Delta
		diff_comp.toggle_diff_mode();
		diff_comp.toggle_diff_mode();
		assert!(diff_comp.is_delta_preview());

		// Narrow preview width: content area = 40 (rect width 42).
		// current_size stores content width/height (rect - 2 borders).
		let rect_width: u16 = 42;
		let rect_height: u16 = 30;
		diff_comp.current_size.set((40, rect_height - 2));
		diff_comp.focus(true);

		let diff = make_filediff(&repo, "f.txt", false);
		assert!(!diff.hunks.is_empty());
		diff_comp.update(
			"f.txt".to_string(),
			false,
			diff.clone(),
			DiffType::WorkDir,
		);
		let params = DeltaParams {
			path: "f.txt".to_string(),
			diff_type: DiffType::WorkDir,
			width: 40,
			side_by_side: false,
			diff_hash: diff_comp.current.hash,
		};
		let result = diff_comp
			.async_delta
			.request_sync(&params, &repo, Some(&diff))
			.expect("delta should produce output");
		diff_comp.apply_delta_result(result);

		let display_len =
			diff_comp.delta_display_lines.borrow().len();
		assert!(
			display_len > 1,
			"should have multiple display lines after wrap, got {display_len}"
		);

		// Render into a TestBackend buffer and count non-empty content
		// rows inside the diff rect.
		let backend = TestBackend::new(rect_width, rect_height);
		let mut terminal = Terminal::new(backend).unwrap();
		terminal
			.draw(|f| {
				diff_comp.draw(f, f.area()).expect("draw failed");
			})
			.unwrap();

		let buffer = terminal.backend().buffer();
		// Find the last non-border row that has content; the
		// trailing wrapped lines of the long CJK line must be
		// visible (not clipped). We check that the last CJK
		// continuation row (the partial 5-char tail) is present.
		let mut saw_cjk_tail = false;
		for y in 1..(rect_height as usize).saturating_sub(1) {
			let mut row_content = String::new();
			for x in 1..(rect_width as usize).saturating_sub(1) {
				let cell = &buffer[(x as u16, y as u16)];
				row_content.push_str(cell.symbol());
			}
			// The wrapped tail row contains 5 CJK "中" chars followed
			// by padding. Count CJK occurrences in the row.
			let cjk_count =
				row_content.chars().filter(|&c| c == '中').count();
			if cjk_count > 0 && cjk_count < 10 {
				saw_cjk_tail = true;
			}
		}

		// The wrapped CJK tail (the partial last row) must be visible.
		// Before the fix, `wrap_line` counted CJK chars as width 1, so
		// the long line wrapped into too few rows and the tail was
		// clipped off-screen.
		assert!(
			saw_cjk_tail,
			"wrapped CJK tail row was not rendered — tail is being truncated (display_len={display_len})"
		);
	}

	#[test]
	fn test_wrap_line_over_width() {
		let line = Line::from(Span::raw("a".repeat(41)));
		let wrapped = delta_wrap_line(&line, 40);
		assert_eq!(wrapped.len(), 2);
	}

	#[test]
	fn test_wrap_line_multi_span_exact_width() {
		let line = Line::from(vec![
			Span::raw("a".repeat(20)),
			Span::raw("b".repeat(20)),
		]);
		let wrapped = delta_wrap_line(&line, 40);
		assert_eq!(
			wrapped.len(),
			1,
			"multi-span exact width should produce 1 line, got {}",
			wrapped.len()
		);
	}

	/// Simulate what rebuild_display_lines does: strip trailing
	/// whitespace-only spans when content exceeds panel width,
	/// then wrap.
	fn simulate_trim_and_wrap(
		line: &Line<'static>,
		panel_width: usize,
	) -> Vec<Line<'static>> {
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
		delta_wrap_line(&trimmed_line, panel_width)
	}

	#[test]
	fn test_blank_line_preserves_background() {
		// Simulate: \x1b[42m \x1b[0K \x1b[0m → green bg space
		// ansi_to_lines produces: Span(" ", bg=green) + Span(" ", bg=green, from \x1b[K])
		let green = Style::default().bg(Color::Green);
		let line = Line::from(vec![
			Span::styled(Cow::Owned(" ".to_string()), green),
			Span::styled(Cow::Owned(" ".to_string()), green),
		]);
		let wrapped = simulate_trim_and_wrap(&line, 40);
		// Blank line should be preserved (not stripped)
		assert_eq!(
			wrapped.len(),
			1,
			"blank line with bg should be 1 line"
		);
		// Should have background color
		let has_bg =
			wrapped[0].spans.iter().any(|s| s.style.bg.is_some());
		assert!(has_bg, "blank line should have background color");
	}

	#[test]
	fn test_full_width_line_no_extra_line() {
		// Simulate: 40 chars of content + 1 trailing space from \x1b[K]
		let green = Style::default().bg(Color::Green);
		let mut spans: Vec<Span<'static>> = (0..40)
			.map(|i| {
				Span::styled(
					Cow::Owned(format!("{}", i % 10)),
					Style::default(),
				)
			})
			.collect();
		// Trailing space from \x1b[K]
		spans.push(Span::styled(Cow::Owned(" ".to_string()), green));
		let line = Line::from(spans);
		let wrapped = simulate_trim_and_wrap(&line, 40);
		assert_eq!(
			wrapped.len(),
			1,
			"full-width line + trailing space should produce 1 line, got {}",
			wrapped.len()
		);
	}

	#[test]
	fn test_over_width_line_wraps_correctly() {
		// 41 chars + trailing space from \x1b[K]
		let green = Style::default().bg(Color::Green);
		let mut spans: Vec<Span<'static>> = (0..41)
			.map(|i| {
				Span::styled(
					Cow::Owned(format!("{}", i % 10)),
					Style::default(),
				)
			})
			.collect();
		spans.push(Span::styled(Cow::Owned(" ".to_string()), green));
		let line = Line::from(spans);
		let wrapped = simulate_trim_and_wrap(&line, 40);
		assert_eq!(
			wrapped.len(),
			2,
			"41 chars should wrap into 2 lines"
		);
	}

	#[test]
	fn test_pad_line_bg_scans_backwards_past_reset_span() {
		// Simulate what delta produces:
		// content with bg, then a reset span (no bg), then need padding
		let bg_style = Style::default().bg(Color::Green);
		let reset_style = Style::default();
		let line = Line::from(vec![
			Span::styled(Cow::Owned("hello".to_string()), bg_style),
			Span::styled(Cow::Owned("".to_string()), reset_style),
		]);
		let padded = delta_pad_line_bg(line, 10, false, None);
		let total_width: usize = padded
			.spans
			.iter()
			.map(|s| s.content.chars().count())
			.sum();
		assert_eq!(total_width, 10, "should be padded to width 10");
		// The padding span should have the bg color
		let last = padded.spans.last().unwrap();
		assert_eq!(
			last.style.bg,
			Some(Color::Green),
			"padding span should carry the bg color"
		);
	}

	#[test]
	fn test_pad_line_bg_long_line_sublines_have_bg() {
		// Simulate a 250-char line that wraps into sublines at width=80
		// All spans have bg color (like delta content lines)
		let bg_style = Style::default().bg(Color::Blue);
		let reset_style = Style::default();
		// Build: [gutter(no bg, 10 chars)] [content(bg, 230 chars)] [reset(no bg)]
		let line = Line::from(vec![
			Span::styled(Cow::Owned("G".repeat(10)), reset_style),
			Span::styled(Cow::Owned("C".repeat(230)), bg_style),
			Span::styled(Cow::Owned("".to_string()), reset_style),
		]);
		let wrapped = delta_wrap_line(&line, 80);
		// Should wrap into ceil(240/80)=3 sublines
		assert_eq!(
			wrapped.len(),
			3,
			"240 chars at width=80 → 3 sublines"
		);

		// Each subline that is less than 80 wide should be paddable
		for (i, subline) in wrapped.iter().enumerate() {
			let w: usize = subline
				.spans
				.iter()
				.map(|s| s.content.chars().count())
				.sum();
			let padded =
				delta_pad_line_bg(subline.clone(), 80, false, None);
			let pw: usize = padded
				.spans
				.iter()
				.map(|s| s.content.chars().count())
				.sum();
			if w < 80 {
				assert_eq!(
					pw, 80,
					"subline {i} (w={w}) should be padded to 80"
				);
				assert_eq!(
					padded.spans.last().unwrap().style.bg,
					Some(Color::Blue),
					"subline {i} padding should have bg color"
				);
			} else {
				assert_eq!(
					pw, w,
					"full subline {i} should not be changed"
				);
			}
		}
	}

	#[test]
	fn test_pad_line_bg_trailing_arrow_gets_bg_even_at_full_width() {
		// SBS delta: [content with bg][→ no-bg], total == panel_width.
		// The → should receive the bg color even though no extra padding is needed.
		let bg_style = Style::default().bg(Color::Green);
		let reset_style = Style::default();
		let line = Line::from(vec![
			Span::styled(
				Cow::Owned("123456789".to_string()),
				bg_style,
			),
			Span::styled(Cow::Owned("→".to_string()), reset_style),
		]);
		let padded = delta_pad_line_bg(line, 10, false, None);
		let arrow =
			padded.spans.iter().find(|s| s.content.as_ref() == "→");
		assert!(arrow.is_some(), "should have → span");
		assert_eq!(
			arrow.unwrap().style.bg,
			Some(Color::Green),
			"→ span should have bg applied"
		);
	}

	#[test]
	fn test_pad_line_bg_sbs_no_bleed_across_panels() {
		// Manually constructed SBS line matching real ansi_to_lines output.
		// ansi_to_lines parses \x1b[34m as Color::Blue (not Indexed(34)),
		// and the last_bg_fg mechanism can overwrite the │ span's fg to rgb.
		let red_bg = Style::default()
			.bg(Color::Rgb(74, 46, 50))
			.fg(Color::Rgb(231, 130, 132));
		let blue_gutter = Style::default().fg(Color::Blue);
		let reset = Style::default();
		let line = Line::from(vec![
			// Left panel gutter
			Span::styled(
				Cow::Owned("\u{2502}".to_string()),
				blue_gutter,
			),
			Span::styled(
				Cow::Owned("  1 ".to_string()),
				Style::default().fg(Color::Indexed(88)),
			),
			Span::styled(
				Cow::Owned("\u{2502}".to_string()),
				blue_gutter,
			),
			// Left panel content with red bg
			Span::styled(
				Cow::Owned("deleted content".to_string()),
				red_bg,
			),
			// Reset span (from \x1b[0m)
			Span::styled(Cow::Owned("".to_string()), reset),
			// Middle separator │ (may have fg overwritten by last_bg_fg)
			Span::styled(
				Cow::Owned("\u{2502}".to_string()),
				Style::default().fg(Color::Rgb(231, 130, 132)),
			),
			// Right panel gutter
			Span::styled(
				Cow::Owned("    ".to_string()),
				Style::default().fg(Color::Indexed(28)),
			),
			Span::styled(
				Cow::Owned("\u{2502}".to_string()),
				blue_gutter,
			),
		]);
		let padded = delta_pad_line_bg(line.clone(), 80, true, None);
		// No gutter/decoration span should have left panel's red bg
		for span in &padded.spans {
			if span.style.bg.is_none()
				|| matches!(
					span.style.bg,
					Some(Color::Rgb(74, 46, 50))
				) {
				// Check │ characters
				if span.content.chars().any(|c| c == '\u{2502}') {
					assert_ne!(
						span.style.bg,
						Some(Color::Rgb(74, 46, 50)),
						"│ gutter should not have left panel bg"
					);
				}
				// Check indexed-fg spans (line numbers)
				if matches!(span.style.fg, Some(Color::Indexed(_))) {
					assert_ne!(
						span.style.bg,
						Some(Color::Rgb(74, 46, 50)),
						"indexed-fg span should not have left panel bg"
					);
				}
				// Check Blue-fg spans (│ gutter)
				if span.style.fg == Some(Color::Blue) {
					assert_ne!(
						span.style.bg,
						Some(Color::Rgb(74, 46, 50)),
						"blue-fg │ should not have left panel bg"
					);
				}
			}
		}
	}

	#[test]
	fn test_pad_line_bg_sbs_right_panel_pads_independently() {
		// SBS delta line: left panel empty, right panel has green bg.
		// The right panel should be padded with its own bg color.
		let blue_gutter = Style::default().fg(Color::Indexed(34));
		let green_bg = Style::default()
			.bg(Color::Rgb(73, 111, 74))
			.fg(Color::Rgb(198, 208, 245));
		let reset = Style::default();
		let line = Line::from(vec![
			// Left panel gutter + empty content
			Span::styled(
				Cow::Owned("\u{2502}".to_string()),
				blue_gutter,
			),
			Span::styled(
				Cow::Owned("    ".to_string()),
				Style::default().fg(Color::Indexed(88)),
			),
			Span::styled(
				Cow::Owned("\u{2502}".to_string()),
				blue_gutter,
			),
			Span::styled(
				Cow::Owned(
					"                                  ".to_string(),
				),
				reset,
			),
			// Right panel gutter
			Span::styled(
				Cow::Owned("\u{2502}".to_string()),
				blue_gutter,
			),
			Span::styled(
				Cow::Owned("  1 ".to_string()),
				Style::default().fg(Color::Indexed(28)),
			),
			Span::styled(
				Cow::Owned("\u{2502}".to_string()),
				blue_gutter,
			),
			// Right panel content with green bg
			Span::styled(
				Cow::Owned("added content".to_string()),
				green_bg,
			),
		]);
		let padded = delta_pad_line_bg(line.clone(), 80, true, None);
		// The last bg span should be green
		let last_bg_span = padded
			.spans
			.iter()
			.find(|s| s.style.bg == Some(Color::Rgb(73, 111, 74)));
		assert!(
			last_bg_span.is_some(),
			"should have green bg content"
		);
		// Padding should be green, not from left panel
		if let Some(pad_span) = padded.spans.last() {
			if pad_span.content.chars().all(|c| c == ' ')
				&& !pad_span.content.is_empty()
			{
				assert_eq!(
					pad_span.style.bg,
					Some(Color::Rgb(73, 111, 74)),
					"padding should have green bg from right panel"
				);
			}
		}
	}

	#[test]
	fn test_pad_line_bg_sbs_gutter_not_affected() {
		// Parse real delta SBS output where left has deleted content,
		// right has only gutter (│    │). After ansi_to_lines, the blue
		// │ characters are merged into adjacent spans, leaving only
		// indexed-color spans with no bg as gutter markers.
		let input = concat!(
			"\x1b[34m\x1b[38;5;88m  1 \x1b[34m\x1b[0m",
			"\x1b[48;2;74;46;50;38;2;231;130;132mdeleted content\x1b[0m",
			"\x1b[34m\x1b[38;5;28m    \x1b[34m\x1b[0m",
			"\n",
		);
		let (parsed, _) = crate::ansi::ansi_to_lines(input);
		assert_eq!(parsed.len(), 1);
		let padded =
			delta_pad_line_bg(parsed[0].clone(), 80, true, None);
		// No span with indexed fg should have left panel's red bg
		for span in &padded.spans {
			if matches!(span.style.fg, Some(Color::Indexed(_))) {
				assert_ne!(
					span.style.bg,
					Some(Color::Rgb(74, 46, 50)),
					"indexed-fg gutter span should not have left panel bg"
				);
			}
		}
	}

	#[test]
	fn test_pad_line_bg_sbs_piped_gutter_not_colored() {
		// Parse real delta SBS output with │ gutter characters.
		// Left has red bg content, right has only gutter (│    │).
		// The │ between panels must not get the left panel's red bg.
		let input = concat!(
			"\x1b[34m\u{2502}\x1b[38;5;88m  1 \x1b[34m\u{2502}\x1b[0m",
			"\x1b[48;2;74;46;50;38;2;231;130;132mdeleted content\x1b[0m",
			"\x1b[34m\u{2502}\x1b[38;5;28m    \x1b[34m\u{2502}\x1b[0m",
			"\n",
		);
		let (parsed, _) = crate::ansi::ansi_to_lines(input);
		assert_eq!(parsed.len(), 1);
		let padded =
			delta_pad_line_bg(parsed[0].clone(), 80, true, None);
		// Every │ span and every indexed-fg span must NOT have red bg
		for span in &padded.spans {
			if span.content.chars().any(|c| c == '\u{2502}') {
				assert_ne!(
					span.style.bg,
					Some(Color::Rgb(74, 46, 50)),
					"│ gutter span must not have left panel bg"
				);
			}
			if matches!(span.style.fg, Some(Color::Indexed(_))) {
				assert_ne!(
					span.style.bg,
					Some(Color::Rgb(74, 46, 50)),
					"indexed-fg gutter span must not have left panel bg"
				);
			}
		}
	}

	#[test]
	fn test_pad_line_bg_sbs_both_panels_with_bg() {
		// Parse delta SBS output where both panels have bg content.
		// Left panel: red bg (deleted), Right panel: green bg (added).
		// Each panel should pad with its own bg color.
		let input = concat!(
			"\x1b[34m\u{2502}\x1b[38;5;88m  1 \x1b[34m\u{2502}\x1b[0m",
			"\x1b[48;2;74;46;50;38;2;231;130;132mdeleted\x1b[0m",
			"\x1b[34m\u{2502}\x1b[38;5;28m  1 \x1b[34m\u{2502}\x1b[0m",
			"\x1b[48;2;73;111;74;38;2;198;208;245madded\x1b[0m",
			"\n",
		);
		let (parsed, _) = crate::ansi::ansi_to_lines(input);
		assert_eq!(parsed.len(), 1);
		let padded =
			delta_pad_line_bg(parsed[0].clone(), 80, true, None);
		// Find the green bg span (right panel)
		let green_bg = padded
			.spans
			.iter()
			.find(|s| s.style.bg == Some(Color::Rgb(73, 111, 74)));
		assert!(green_bg.is_some(), "should have green bg content");
		// Find the red bg span (left panel)
		let red_bg = padded
			.spans
			.iter()
			.find(|s| s.style.bg == Some(Color::Rgb(74, 46, 50)));
		assert!(red_bg.is_some(), "should have red bg content");
		// │ gutter spans must not have red bg
		for span in &padded.spans {
			if span.content.chars().any(|c| c == '\u{2502}') {
				assert_ne!(
					span.style.bg,
					Some(Color::Rgb(74, 46, 50)),
					"│ must not have left panel bg"
				);
			}
		}
	}

	#[test]
	fn test_pad_line_bg_sbs_arrow_still_gets_bg() {
		// In non-SBS mode (or within a panel), the ↵ arrow after bg
		// content should still receive the bg color. SBS boundary
		// detection must not break this.
		let bg_style = Style::default()
			.bg(Color::Rgb(74, 46, 50))
			.fg(Color::Rgb(231, 130, 132));
		let reset = Style::default();
		let line = Line::from(vec![
			Span::styled(Cow::Owned("content".to_string()), bg_style),
			Span::styled(Cow::Owned("\u{21b5}".to_string()), reset),
		]);
		// Non-SBS: arrow gets bg
		let padded = delta_pad_line_bg(line.clone(), 20, false, None);
		let arrow = padded
			.spans
			.iter()
			.find(|s| s.content.chars().any(|c| c == '\u{21b5}'));
		assert!(arrow.is_some());
		assert_eq!(
			arrow.unwrap().style.bg,
			Some(Color::Rgb(74, 46, 50)),
			"↵ arrow should get bg in non-SBS mode"
		);
	}

	#[test]
	fn test_pad_line_bg_uses_dominant_bg_not_word_highlight() {
		// A wrapped sub-line where only "ANGED" (word highlight gray bg)
		// has bg. The dominant_bg from the original line (red) is passed
		// explicitly. pad_line_bg should use red for padding.
		let gray_bg = Style::default()
			.bg(Color::Rgb(204, 204, 204))
			.fg(Color::Rgb(255, 0, 0));
		let line = Line::from(vec![
			// Word highlight span (the only bg on this sub-line)
			Span::styled(Cow::Owned("ANGED".to_string()), gray_bg),
		]);
		let padded = delta_pad_line_bg(
			line,
			35,
			false,
			Some(Color::Rgb(74, 46, 50)),
		);
		// Padding span should have red bg (dominant), not gray bg (word highlight)
		for span in &padded.spans {
			if span.content.chars().all(|c| c == ' ')
				&& !span.content.is_empty()
			{
				assert_eq!(
					span.style.bg,
					Some(Color::Rgb(74, 46, 50)),
					"padding should use dominant (red) bg, not word-highlight (gray) bg"
				);
			}
		}
	}
}
