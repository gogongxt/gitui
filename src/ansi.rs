use ratatui::{
	style::{Color, Modifier, Style},
	text::{Line, Span},
};
use std::borrow::Cow;

/// When delta outputs a long line it sometimes resets styles mid-line and
/// restores only bg, not fg. After a line's spans are fully parsed we scan
/// them to find the dominant fg color (the first rgb fg seen while bg is
/// active) and back-fill any bg spans that are missing fg.
///
/// `fallback_fg` carries the rgb fg from the previous line, used when delta
/// wraps a long line across multiple output lines and the continuation lines
/// have no rgb fg of their own.
fn normalize_line_fg(
	spans: &mut Vec<Span<'static>>,
	fallback_fg: Option<Color>,
) {
	// Prefer an rgb fg found in this line; fall back to the previous line's fg.
	let content_fg: Option<Color> = spans
		.iter()
		.filter(|s| s.style.bg.is_some())
		.find_map(|s| match s.style.fg {
			Some(Color::Rgb(_, _, _)) => s.style.fg,
			_ => None,
		})
		.or(fallback_fg);
	let Some(fg) = content_fg else {
		return;
	};
	for span in spans.iter_mut() {
		if span.style.bg.is_some() && span.style.fg.is_none() {
			span.style = span.style.fg(fg);
		}
	}
}

/// Convert ANSI-colored text into ratatui `Line`s.
///
/// Returns `(lines, line_level_bgs)` where `line_level_bgs[i]` is the
/// line-level background color for `lines[i]`. This is the bg color
/// that delta intends for the *entire* line (red for deletions, green
/// for additions), as opposed to transient word-diff highlight bgs.
///
/// Detection heuristic: delta always clears word-diff highlights before
/// a line ends. The last bg color actively set before each newline (or
/// end-of-input) is the line-level bg.
pub fn ansi_to_lines(
	input: &str,
) -> (Vec<Line<'static>>, Vec<Option<Color>>) {
	let mut lines: Vec<Line<'static>> = Vec::new();
	let mut lines_level_bg: Vec<Option<Color>> = Vec::new();
	let mut current_style = Style::default();
	let mut current_spans: Vec<Span<'static>> = Vec::new();
	let mut buf = String::new();
	// Track the last *rgb* fg color seen while a bg was active. Delta
	// sometimes resets styles mid-line and restores bg but not fg (e.g.
	// for long lines crossing a syntax-highlight boundary).  We only
	// track rgb fg — std/indexed colors (like the blue gutter `│`) are
	// decorations, not content colors.
	let mut last_bg_fg: Option<Color> = None;
	// Track the last bg color that was actively set (not reset). At each
	// line boundary this becomes the line-level bg for that line. We do
	// NOT update this on reset — so it retains the line-level bg through
	// word-diff overlays and the final \x1b[0m.
	let mut last_bg_set: Option<Color> = None;
	let mut chars = input.chars().peekable();

	while let Some(c) = chars.next() {
		if c == '\x1b' {
			// Flush buffered text
			if !buf.is_empty() {
				current_spans.push(Span::styled(
					Cow::Owned(buf.clone()),
					current_style,
				));
				buf.clear();
			}

			if chars.peek() == Some(&'[') {
				chars.next(); // consume '['
				let mut params = String::new();
				let mut final_char = None;
				while let Some(&next) = chars.peek() {
					if next.is_ascii_digit() || next == ';' {
						params.push(next);
						chars.next();
					} else {
						final_char = Some(next);
						chars.next(); // consume final char
						break;
					}
				}
				// Only process SGR sequences (ending with 'm')
				if final_char == Some('m') && !params.is_empty() {
					current_style = apply_sgr(current_style, &params);
					// Track the last real bg color that was set.
					// On reset (code 0) or bg-reset (code 49), bg
					// becomes Reset/None — we do NOT update last_bg_set,
					// so it retains the previous line-level bg.
					if let Some(bg) = current_style.bg {
						if bg != Color::Reset {
							last_bg_set = Some(bg);
						}
					}
					// Delta sometimes resets styles mid-line and restores bg
					// but not fg. Track the last fg seen while bg was active,
					// and re-apply it when bg comes back without an explicit fg.
					if current_style.bg.is_some() {
						if let Some(fg) = current_style.fg {
							// Only track rgb fg — std/indexed colors
							// (like blue gutter `│`) are decorations,
							// not content colors.
							if matches!(fg, Color::Rgb(_, _, _)) {
								last_bg_fg = Some(fg);
							}
						} else if let Some(inherited) = last_bg_fg {
							// bg is set but fg was cleared by a prior reset —
							// restore the last known fg color.
							current_style =
								current_style.fg(inherited);
						}
					}
					// When bg is cleared (reset or explicit bg-reset) we keep
					// last_bg_fg so it survives a \x1b[0m...\x1b[48;...m pair.
					// It is reset only at line boundaries (newline handling below).
				}
				// Erase-in-line (K): add a space so the background color renders
				if final_char == Some('K')
					&& current_style.bg.is_some()
				{
					buf.push(' ');
				}
			}
		} else if c == '\n' {
			// Flush and start new line
			if !buf.is_empty() {
				current_spans.push(Span::styled(
					Cow::Owned(buf.clone()),
					current_style,
				));
				buf.clear();
			}
			normalize_line_fg(&mut current_spans, last_bg_fg);
			// Update last_bg_fg from this line for the next line to inherit.
			// Delta wraps long lines across multiple output lines; each wrapped
			// continuation shares the same fg as the first segment.
			if let Some(new_fg) = current_spans
				.iter()
				.filter(|s| s.style.bg.is_some())
				.find_map(|s| match s.style.fg {
					Some(Color::Rgb(_, _, _)) => s.style.fg,
					_ => None,
				}) {
				last_bg_fg = Some(new_fg);
			} else if current_spans
				.iter()
				.all(|s| s.style.bg.is_none())
			{
				// Line with no bg content (separator/header): clear fg memory.
				last_bg_fg = None;
			}
			lines_level_bg.push(last_bg_set);
			last_bg_set = None;
			lines
				.push(Line::from(std::mem::take(&mut current_spans)));
		} else if c == '\r' {
			// ignore CR
		} else {
			buf.push(c);
		}
	}

	// Flush remaining
	if !buf.is_empty() {
		current_spans
			.push(Span::styled(Cow::Owned(buf), current_style));
	}
	if !current_spans.is_empty() {
		normalize_line_fg(&mut current_spans, last_bg_fg);
		lines_level_bg.push(last_bg_set);
		lines.push(Line::from(current_spans));
	}

	(lines, lines_level_bg)
}

#[allow(clippy::too_many_lines, clippy::cognitive_complexity)]
fn apply_sgr(style: Style, params: &str) -> Style {
	let codes: Vec<u32> =
		params.split(';').filter_map(|s| s.parse().ok()).collect();

	if codes.is_empty() {
		return style;
	}

	let mut i = 0;
	let mut result = style;

	while i < codes.len() {
		match codes[i] {
			0 => result = Style::default(),
			1 => result = result.add_modifier(Modifier::BOLD),
			2 => result = result.add_modifier(Modifier::DIM),
			3 => result = result.add_modifier(Modifier::ITALIC),
			4 => result = result.add_modifier(Modifier::UNDERLINED),
			// reverse (7) and strikethrough (9) — not supported
			22 => {
				result = result
					.remove_modifier(Modifier::BOLD | Modifier::DIM);
			}
			23 => result = result.remove_modifier(Modifier::ITALIC),
			24 => {
				result = result.remove_modifier(Modifier::UNDERLINED);
			}
			30..=37 => {
				result =
					result.fg(ansi_standard_color(codes[i] - 30));
			}
			38 => {
				if let Some((color, consumed)) =
					parse_extended_color(&codes[i + 1..])
				{
					result = result.fg(color);
					i += consumed;
				}
			}
			39 => result = result.fg(Color::Reset),
			40..=47 => {
				result =
					result.bg(ansi_standard_color(codes[i] - 40));
			}
			48 => {
				if let Some((color, consumed)) =
					parse_extended_color(&codes[i + 1..])
				{
					result = result.bg(color);
					i += consumed;
				}
			}
			49 => result = result.bg(Color::Reset),
			90..=97 => {
				result = result.fg(ansi_bright_color(codes[i] - 90));
			}
			100..=107 => {
				result = result.bg(ansi_bright_color(codes[i] - 100));
			}
			_ => {} // unknown code, skip
		}
		i += 1;
	}

	result
}

fn parse_extended_color(params: &[u32]) -> Option<(Color, usize)> {
	if params.is_empty() {
		return None;
	}
	match params[0] {
		5 if params.len() >= 2 => Some((
			Color::Indexed(u8::try_from(params[1]).unwrap_or(0)),
			2,
		)),
		2 if params.len() >= 4 => Some((
			Color::Rgb(
				u8::try_from(params[1]).unwrap_or(0),
				u8::try_from(params[2]).unwrap_or(0),
				u8::try_from(params[3]).unwrap_or(0),
			),
			4,
		)),
		_ => None,
	}
}

const fn ansi_standard_color(code: u32) -> Color {
	match code {
		0 => Color::Black,
		1 => Color::Red,
		2 => Color::Green,
		3 => Color::Yellow,
		4 => Color::Blue,
		5 => Color::Magenta,
		6 => Color::Cyan,
		_ => Color::White,
	}
}

const fn ansi_bright_color(code: u32) -> Color {
	match code {
		0 => Color::Gray,
		1 => Color::LightRed,
		2 => Color::LightGreen,
		3 => Color::LightYellow,
		4 => Color::LightBlue,
		5 => Color::LightMagenta,
		6 => Color::LightCyan,
		_ => Color::White,
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn test_plain_text() {
		let (lines, _) = ansi_to_lines("hello world");
		assert_eq!(lines.len(), 1);
		let spans = &lines[0].spans;
		assert_eq!(spans.len(), 1);
		assert_eq!(spans[0].content.as_ref(), "hello world");
	}

	#[test]
	fn test_multiline() {
		let (lines, _) = ansi_to_lines("line1\nline2\nline3");
		assert_eq!(lines.len(), 3);
		assert_eq!(lines[0].spans[0].content.as_ref(), "line1");
		assert_eq!(lines[1].spans[0].content.as_ref(), "line2");
		assert_eq!(lines[2].spans[0].content.as_ref(), "line3");
	}

	#[test]
	fn test_sgr_color() {
		// Red foreground
		let (lines, _) = ansi_to_lines("\x1b[31mred\x1b[0m");
		assert_eq!(lines.len(), 1);
		let spans = &lines[0].spans;
		assert_eq!(spans.len(), 1);
		assert_eq!(spans[0].content.as_ref(), "red");
		assert_eq!(spans[0].style.fg, Some(Color::Red));
	}

	#[test]
	fn test_sgr_rgb_color() {
		// RGB foreground
		let (lines, _) =
			ansi_to_lines("\x1b[38;2;200;100;50mrgb\x1b[0m");
		assert_eq!(lines.len(), 1);
		let spans = &lines[0].spans;
		assert_eq!(spans.len(), 1);
		assert_eq!(spans[0].content.as_ref(), "rgb");
		assert_eq!(spans[0].style.fg, Some(Color::Rgb(200, 100, 50)));
	}

	#[test]
	fn test_sgr_background() {
		// Green background
		let (lines, _) = ansi_to_lines("\x1b[42mgreen bg\x1b[0m");
		assert_eq!(lines.len(), 1);
		let spans = &lines[0].spans;
		assert_eq!(spans[0].style.bg, Some(Color::Green));
	}

	#[test]
	fn test_erase_in_line_ignored() {
		// \x1b[0K should be ignored (erase in line)
		let (lines, _) =
			ansi_to_lines("\x1b[48;2;73;111;74m+\x1b[0m\x1b[48;2;73;111;74m\x1b[0K\x1b[0m");
		assert_eq!(lines.len(), 1);
		let spans = &lines[0].spans;
		// Should have "+" span with green bg, then a reset span
		assert!(spans.len() >= 1);
		// The "+" should be present
		let has_plus =
			spans.iter().any(|s| s.content.as_ref() == "+");
		assert!(has_plus, "should contain '+'");
		// Should not contain any raw escape codes
		for span in spans {
			assert!(
				!span.content.contains('\x1b'),
				"should not contain raw escape: {:?}",
				span.content
			);
		}
	}

	#[test]
	fn test_delta_style_output() {
		// Simulate real delta output
		let input = "\x1b[48;2;73;111;74m+\x1b[38;2;202;158;230mmod\x1b[38;2;198;208;245m \x1b[38;2;239;159;118mansi\x1b[38;2;148;156;187m;\x1b[0m\x1b[48;2;73;111;74m\x1b[0K\x1b[0m";
		let (lines, _) = ansi_to_lines(input);
		assert_eq!(lines.len(), 1);
		let spans = &lines[0].spans;
		// Should have multiple spans with different colors
		assert!(
			spans.len() >= 4,
			"expected >= 4 spans, got {}",
			spans.len()
		);
		// All content should be clean (no escape codes)
		for span in spans {
			assert!(!span.content.contains('\x1b'));
		}
		// First span should have green background (the "+")
		assert_eq!(spans[0].content.as_ref(), "+");
		assert_eq!(spans[0].style.bg, Some(Color::Rgb(73, 111, 74)));
	}

	#[test]
	fn test_mixed_content_and_escapes() {
		let input = "plain\x1b[1mbold\x1b[0mplain";
		let (lines, _) = ansi_to_lines(input);
		assert_eq!(lines.len(), 1);
		let spans = &lines[0].spans;
		assert_eq!(spans.len(), 3);
		assert_eq!(spans[0].content.as_ref(), "plain");
		assert_eq!(spans[1].content.as_ref(), "bold");
		assert!(spans[1].style.add_modifier.contains(Modifier::BOLD));
		assert_eq!(spans[2].content.as_ref(), "plain");
	}

	#[test]
	fn test_full_delta_pipeline() {
		// Run actual git diff | delta and verify output is parseable
		let git_output = std::process::Command::new("git")
			.args(["diff", "--", "src/main.rs"])
			.output();
		if git_output.is_err() {
			return; // skip if not in a git repo
		}
		let git_output = git_output.unwrap();
		if !git_output.status.success()
			|| git_output.stdout.is_empty()
		{
			return; // skip if no diff
		}

		let delta_result = std::process::Command::new("delta")
			.args(["--width", "80"])
			.stdin(std::process::Stdio::piped())
			.stdout(std::process::Stdio::piped())
			.stderr(std::process::Stdio::null())
			.spawn()
			.and_then(|mut child| {
				use std::io::Write;
				if let Some(mut stdin) = child.stdin.take() {
					let _ = stdin.write_all(&git_output.stdout);
				}
				child.wait_with_output()
			});

		if let Ok(delta_output) = delta_result {
			if delta_output.status.success() {
				let text =
					String::from_utf8_lossy(&delta_output.stdout);
				let (lines, _) = ansi_to_lines(&text);
				assert!(
					!lines.is_empty(),
					"delta output should produce at least one line"
				);
				// Verify no raw escape codes leaked through
				for line in &lines {
					for span in &line.spans {
						assert!(
							!span.content.contains('\x1b'),
							"raw escape in span: {:?}",
							span.content
						);
					}
				}
			}
		}
	}

	#[test]
	fn test_fg_inherited_after_reset_with_bg_restore() {
		// Simulate delta mid-line reset: sets bg+fg, resets, restores bg but not fg.
		// \x1b[48;2;0;40;0;38;2;200;200;200m  → bg+fg set
		// first\x1b[0m                          → reset
		// \x1b[48;2;0;40;0m                     → only bg restored
		// second\x1b[0m
		let input = "\x1b[48;2;0;40;0m\x1b[38;2;200;200;200mfirst\x1b[0m\x1b[48;2;0;40;0msecond\x1b[0m";
		let (lines, _) = ansi_to_lines(input);
		assert_eq!(lines.len(), 1);
		let spans = &lines[0].spans;
		let first =
			spans.iter().find(|s| s.content.as_ref() == "first");
		let second =
			spans.iter().find(|s| s.content.as_ref() == "second");
		assert!(first.is_some(), "should have 'first' span");
		assert!(second.is_some(), "should have 'second' span");
		let first_fg = first.unwrap().style.fg;
		let second_fg = second.unwrap().style.fg;
		assert_eq!(
			first_fg,
			Some(Color::Rgb(200, 200, 200)),
			"first span should have fg"
		);
		assert_eq!(
			second_fg, first_fg,
			"second span should inherit fg from first (delta mid-line reset bug)"
		);
	}

	#[test]
	fn test_normalize_line_fg_fills_bg_spans_missing_fg() {
		// Simulate SBS delta: content with bg+rgb-fg, then reset, then
		// more content with bg only (no fg).  normalize_line_fg should
		// back-fill the missing fg from the rgb fg span.
		// \x1b[48;2;0;40;0;38;2;198;208;245mcontent\x1b[0m
		// \x1b[48;2;0;40;0mnofg\x1b[0m
		let input = "\x1b[48;2;0;40;0m\x1b[38;2;198;208;245mcontent\x1b[0m\x1b[48;2;0;40;0mnofg\x1b[0m";
		let (lines, _) = ansi_to_lines(input);
		assert_eq!(lines.len(), 1);
		let spans = &lines[0].spans;
		let content =
			spans.iter().find(|s| s.content.as_ref() == "content");
		let nofg =
			spans.iter().find(|s| s.content.as_ref() == "nofg");
		assert!(content.is_some());
		assert!(nofg.is_some());
		let expected_fg = Some(Color::Rgb(198, 208, 245));
		assert_eq!(
			content.unwrap().style.fg,
			expected_fg,
			"content span should have rgb fg"
		);
		assert_eq!(
			nofg.unwrap().style.fg,
			expected_fg,
			"nofg span should be back-filled with the same rgb fg"
		);
	}

	#[test]
	fn test_normalize_line_fg_ignores_standard_color_gutter() {
		// Gutter uses std color 34 (blue) which should NOT be used as
		// content_fg. Only rgb fg should be considered.
		// delta resets after each gutter segment, then sets only bg.
		// Since there is no rgb fg in any bg span, content keeps fg=None.
		let input =
			"\x1b[34m\u{2502}\x1b[0m\x1b[48;2;0;40;0mnofg\x1b[0m";
		let (lines, _) = ansi_to_lines(input);
		assert_eq!(lines.len(), 1);
		let spans = &lines[0].spans;
		let nofg =
			spans.iter().find(|s| s.content.as_ref() == "nofg");
		assert!(nofg.is_some());
		assert_eq!(
			nofg.unwrap().style.fg,
			None,
			"nofg span should keep fg=None when no rgb fg exists"
		);
	}

	#[test]
	fn test_cross_line_fg_inheritance_for_sbs_wrap() {
		// Simulate delta SBS wrapping a long line across two output lines.
		// Real delta pattern: gutter uses blue + reset, then content uses
		// bg+rgb-fg (first line) or bg only (continuation, fg=None after reset).
		let input = concat!(
			"\x1b[34m\u{2502}\x1b[38;5;28m 16 \x1b[34m\u{2502}\x1b[0m",
			"\x1b[48;2;0;40;0;38;2;198;208;245mpart one",
			"\x1b[34m\u{21b5}\x1b[0m\n",
			"\x1b[34m\u{2502}\x1b[38;5;28m    \x1b[34m\u{2502}\x1b[0m",
			"\x1b[48;2;0;40;0mpart two",
			"\x1b[34m\u{21b5}\x1b[0m\n",
		);
		let (lines, _) = ansi_to_lines(input);
		assert_eq!(lines.len(), 2);

		let spans1 = &lines[0].spans;
		let part1 =
			spans1.iter().find(|s| s.content.as_ref() == "part one");
		assert!(part1.is_some());
		assert_eq!(
			part1.unwrap().style.fg,
			Some(Color::Rgb(198, 208, 245)),
			"line 1 content should have rgb fg"
		);

		let spans2 = &lines[1].spans;
		let part2 =
			spans2.iter().find(|s| s.content.as_ref() == "part two");
		assert!(part2.is_some());
		assert_eq!(
			part2.unwrap().style.fg,
			Some(Color::Rgb(198, 208, 245)),
			"line 2 continuation should inherit rgb fg from line 1"
		);
	}

	#[test]
	fn test_cross_line_fg_inheritance_stops_at_separator() {
		// A separator/header line (no bg content) should clear last_bg_fg,
		// preventing leakage into unrelated content below.
		let input = concat!(
			"\x1b[34m\u{2502}\x1b[38;5;28m 16 \x1b[34m\u{2502}\x1b[0m",
			"\x1b[48;2;0;40;0;38;2;198;208;245mreal content",
			"\x1b[34m\u{21b5}\x1b[0m\n",
			"\x1b[34m",
			"\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}",
			"\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}",
			"\x1b[0m\n",
			"\x1b[34m\u{2502}\x1b[38;5;28m  1 \x1b[34m\u{2502}\x1b[0m",
			"\x1b[48;2;0;40;0mother content",
			"\x1b[34m\u{21b5}\x1b[0m\n",
		);
		let (lines, _) = ansi_to_lines(input);
		assert_eq!(lines.len(), 3);

		let spans3 = &lines[2].spans;
		let other = spans3
			.iter()
			.find(|s| s.content.as_ref() == "other content");
		assert!(other.is_some());
		assert_eq!(
			other.unwrap().style.fg,
			None,
			"fg should not leak past a separator line"
		);
	}

	#[test]
	fn test_line_level_bg_word_diff_mid_line() {
		// Delta: line-level red bg, word-diff gray bg on changed word.
		// After word-diff, delta restores line-level bg (red) before
		// the final reset. The line-level bg should be detected as red.
		let input = concat!(
			"\x1b[48;2;74;46;50m", // line-level red bg
			"hello ",
			"\x1b[48;2;204;204;204m", // word-diff gray bg
			"world",
			"\x1b[48;2;74;46;50m", // restore line-level red bg
			"  ",
			"\x1b[0m\n", // reset + newline
		);
		let (lines, bgs) = ansi_to_lines(input);
		assert_eq!(lines.len(), 1);
		assert_eq!(bgs.len(), 1);
		// Line-level bg should be red (the last real bg set before end-of-line)
		assert_eq!(
			bgs[0],
			Some(Color::Rgb(74, 46, 50)),
			"line-level bg should be red, not word-diff gray"
		);
	}

	#[test]
	fn test_line_level_bg_word_diff_at_start() {
		// Delta: word-diff gray bg at start, then line-level red bg
		// The line-level bg should be detected as red.
		let input = concat!(
			"\x1b[48;2;204;204;204m", // word-diff gray bg (first word changed)
			"hello",
			"\x1b[48;2;74;46;50m", // line-level red bg
			" world",
			"\x1b[0m\n",
		);
		let (lines, bgs) = ansi_to_lines(input);
		assert_eq!(lines.len(), 1);
		assert_eq!(
			bgs[0],
			Some(Color::Rgb(74, 46, 50)),
			"line-level bg should be red even when word-diff appears first"
		);
	}

	#[test]
	fn test_line_level_bg_no_bg() {
		// Line with no bg (separator/header) should have None
		let input = "\x1b[34m---\x1b[0m\n";
		let (lines, bgs) = ansi_to_lines(input);
		assert_eq!(lines.len(), 1);
		assert_eq!(bgs[0], None, "separator line should have no bg");
	}

	#[test]
	fn test_line_level_bg_multiple_lines() {
		// Two lines: first with red bg, second with green bg
		let input = concat!(
			"\x1b[48;2;74;46;50mdeleted\x1b[0m\n",
			"\x1b[48;2;73;111;74madded\x1b[0m\n",
		);
		let (lines, bgs) = ansi_to_lines(input);
		assert_eq!(lines.len(), 2);
		assert_eq!(bgs.len(), 2);
		assert_eq!(bgs[0], Some(Color::Rgb(74, 46, 50)));
		assert_eq!(bgs[1], Some(Color::Rgb(73, 111, 74)));
	}

	#[test]
	fn test_line_level_bg_with_erase_in_line() {
		// Delta: content + \x1b[K] (erase-in-line) with line-level bg
		let input = concat!(
			"\x1b[48;2;73;111;74m",
			"+",
			"\x1b[38;2;202;158;230mmod",
			"\x1b[0m",
			"\x1b[48;2;73;111;74m",
			"\x1b[0K", // erase-in-line
			"\x1b[0m\n",
		);
		let (lines, bgs) = ansi_to_lines(input);
		assert_eq!(lines.len(), 1);
		assert_eq!(
			bgs[0],
			Some(Color::Rgb(73, 111, 74)),
			"line-level bg should be green (from \x1b[K] region)"
		);
	}
}
