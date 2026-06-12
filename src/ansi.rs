use ratatui::{
	style::{Color, Modifier, Style},
	text::{Line, Span},
};
use std::borrow::Cow;

/// Convert ANSI-colored text into ratatui `Line`s.
pub fn ansi_to_lines(input: &str) -> Vec<Line<'static>> {
	let mut lines: Vec<Line<'static>> = Vec::new();
	let mut current_style = Style::default();
	let mut current_spans: Vec<Span<'static>> = Vec::new();
	let mut buf = String::new();
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
		lines.push(Line::from(current_spans));
	}

	lines
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
				result = result.remove_modifier(Modifier::UNDERLINED)
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
		let lines = ansi_to_lines("hello world");
		assert_eq!(lines.len(), 1);
		let spans = &lines[0].spans;
		assert_eq!(spans.len(), 1);
		assert_eq!(spans[0].content.as_ref(), "hello world");
	}

	#[test]
	fn test_multiline() {
		let lines = ansi_to_lines("line1\nline2\nline3");
		assert_eq!(lines.len(), 3);
		assert_eq!(lines[0].spans[0].content.as_ref(), "line1");
		assert_eq!(lines[1].spans[0].content.as_ref(), "line2");
		assert_eq!(lines[2].spans[0].content.as_ref(), "line3");
	}

	#[test]
	fn test_sgr_color() {
		// Red foreground
		let lines = ansi_to_lines("\x1b[31mred\x1b[0m");
		assert_eq!(lines.len(), 1);
		let spans = &lines[0].spans;
		assert_eq!(spans.len(), 1);
		assert_eq!(spans[0].content.as_ref(), "red");
		assert_eq!(spans[0].style.fg, Some(Color::Red));
	}

	#[test]
	fn test_sgr_rgb_color() {
		// RGB foreground
		let lines = ansi_to_lines("\x1b[38;2;200;100;50mrgb\x1b[0m");
		assert_eq!(lines.len(), 1);
		let spans = &lines[0].spans;
		assert_eq!(spans.len(), 1);
		assert_eq!(spans[0].content.as_ref(), "rgb");
		assert_eq!(spans[0].style.fg, Some(Color::Rgb(200, 100, 50)));
	}

	#[test]
	fn test_sgr_background() {
		// Green background
		let lines = ansi_to_lines("\x1b[42mgreen bg\x1b[0m");
		assert_eq!(lines.len(), 1);
		let spans = &lines[0].spans;
		assert_eq!(spans[0].style.bg, Some(Color::Green));
	}

	#[test]
	fn test_erase_in_line_ignored() {
		// \x1b[0K should be ignored (erase in line)
		let lines =
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
		let lines = ansi_to_lines(input);
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
		let lines = ansi_to_lines(input);
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
				let lines = ansi_to_lines(&text);
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
}
