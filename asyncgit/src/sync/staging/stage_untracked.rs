use super::load_file;
use crate::{
	error::Result,
	sync::{diff::DiffLinePosition, repository::repo, RepoPath},
};
use easy_cast::Conv;
use git2::{DiffLineType, Patch};
use scopetime::scope_time;
use std::{collections::HashSet, path::Path};

///
pub fn stage_lines_untracked(
	repo_path: &RepoPath,
	file_path: &str,
	lines: &[DiffLinePosition],
) -> Result<()> {
	scope_time!("stage_lines_untracked");

	if lines.is_empty() {
		return Ok(());
	}

	let repo = repo(repo_path)?;
	let work_dir = repo.workdir().ok_or_else(|| {
		crate::error::Error::Generic(String::from("no workdir"))
	})?;

	let file_content = load_file(&repo, file_path)?;
	let file_content_bytes = file_content.as_bytes();

	let path = Path::new(file_path);
	let newfile_path = work_dir.join(path);

	let patch = Patch::from_buffers(
		&[],
		None,
		file_content_bytes,
		Some(newfile_path.as_path()),
		None,
	)?;

	// Build set of selected line positions for fast lookup
	let selected: HashSet<DiffLinePosition> =
		lines.iter().cloned().collect();

	// For an untracked file, all diff lines are additions.
	// Collect only the selected addition lines.
	let mut selected_content = String::new();
	let num_hunks = patch.num_hunks();
	for hunk_idx in 0..num_hunks {
		let num_lines = patch.num_lines_in_hunk(hunk_idx)?;
		for line_idx in 0..num_lines {
			let line = patch.line_in_hunk(hunk_idx, line_idx)?;

			if line.origin_value() == DiffLineType::Addition {
				let pos = DiffLinePosition {
					old_lineno: None,
					new_lineno: line
						.new_lineno()
						.and_then(|n| u32::try_from(n).ok()),
				};
				if selected.contains(&pos) {
					let content =
						String::from_utf8_lossy(line.content());
					// Strip trailing newline if present; we'll
					// ensure a final newline at the end.
					selected_content
						.push_str(content.trim_end_matches('\n'));
					selected_content.push('\n');
				}
			}
		}
	}

	// No lines selected - nothing to do
	if selected_content.is_empty() {
		return Ok(());
	}

	let mut index = repo.index()?;
	index.read(true)?;

	let blob_id = repo.blob(selected_content.as_bytes())?;

	let idx = git2::IndexEntry {
		ctime: git2::IndexTime::new(0, 0),
		mtime: git2::IndexTime::new(0, 0),
		dev: 0,
		ino: 0,
		mode: 0o100644,
		uid: 0,
		gid: 0,
		file_size: u32::try_conv(selected_content.len())?,
		id: blob_id,
		flags: 0,
		flags_extended: 0,
		path: file_path.as_bytes().to_vec(),
	};

	index.add(&idx)?;
	index.write()?;

	Ok(())
}

#[cfg(test)]
mod test {
	use super::*;
	use crate::sync::{
		diff::get_diff,
		tests::{get_statuses, repo_init},
		utils::repo_write_file,
	};

	#[test]
	fn test_stage_lines_untracked() {
		let (path, _repo) = repo_init().unwrap();
		let path: &RepoPath = &path.path().to_str().unwrap().into();

		repo_write_file(&_repo, "new.txt", "line1\nline2\nline3\n")
			.unwrap();

		assert_eq!(get_statuses(path), (1, 0));

		// Stage only line2
		stage_lines_untracked(
			path,
			"new.txt",
			&[DiffLinePosition {
				old_lineno: None,
				new_lineno: Some(2),
			}],
		)
		.unwrap();

		// Now the file should be partially staged
		let diff = get_diff(path, "new.txt", true, None).unwrap();

		// The staged diff should show line2 as an addition
		// diff.lines counts hunk header + content lines
		assert_eq!(diff.lines, 2);
	}

	#[test]
	fn test_stage_all_lines_untracked() {
		let (path, _repo) = repo_init().unwrap();
		let path: &RepoPath = &path.path().to_str().unwrap().into();

		repo_write_file(&_repo, "new.txt", "line1\nline2\nline3\n")
			.unwrap();

		// Stage all lines
		stage_lines_untracked(
			path,
			"new.txt",
			&[
				DiffLinePosition {
					old_lineno: None,
					new_lineno: Some(1),
				},
				DiffLinePosition {
					old_lineno: None,
					new_lineno: Some(2),
				},
				DiffLinePosition {
					old_lineno: None,
					new_lineno: Some(3),
				},
			],
		)
		.unwrap();

		// The staged diff should show all 3 lines as additions
		// (header + 3 content lines)
		let staged_diff =
			get_diff(path, "new.txt", true, None).unwrap();
		assert_eq!(staged_diff.lines, 4);

		// And no remaining unstaged changes
		let unstaged_diff =
			get_diff(path, "new.txt", false, None).unwrap();
		assert_eq!(unstaged_diff.lines, 0);
	}
}
