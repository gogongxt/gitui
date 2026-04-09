use super::{CommitId, RepoPath};
use crate::{error::Result, sync::repository::repo};
use scopetime::scope_time;

/// Reflog action type
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReflogAction {
	/// commit: created a new commit
	Commit,
	/// checkout: moving from X to Y
	Checkout {
		/// source branch/commit
		from: Option<String>,
		/// target branch/commit
		to: Option<String>,
	},
	/// reset: moving to X
	Reset {
		/// target
		to: Option<String>,
	},
	/// rebase: (various phases)
	Rebase,
	/// merge: created a merge commit
	Merge,
	/// pull: fetched and merged/rebased
	Pull,
	/// clone: cloned repository
	Clone,
	/// branch: created/deleted branch
	Branch,
	/// amend: amended a commit
	Amend,
	/// cherry-pick: cherry-picked a commit
	CherryPick,
	/// Unknown action
	Unknown,
}

impl ReflogAction {
	/// Parse action from reflog message
	pub fn from_message(message: &str) -> Self {
		// Handle empty message
		if message.is_empty() {
			return Self::Unknown;
		}

		let message_lower = message.to_lowercase();

		if message_lower.starts_with("commit:") {
			Self::Commit
		} else if message_lower.starts_with("checkout:") {
			// Parse "checkout: moving from X to Y"
			let rest =
				message.strip_prefix("checkout:").unwrap_or(message);
			let rest = rest.trim();

			let (from, to) = if rest.starts_with("moving from") {
				let rest =
					rest.strip_prefix("moving from").unwrap_or(rest);
				let parts: Vec<&str> = rest.split(" to ").collect();
				if parts.len() == 2 {
					(
						Some(parts[0].trim().to_string()),
						Some(parts[1].trim().to_string()),
					)
				} else {
					(None, None)
				}
			} else {
				// Just "checkout: X" format
				(None, Some(rest.to_string()))
			};

			Self::Checkout { from, to }
		} else if message_lower.starts_with("reset:") {
			// Parse "reset: moving to X"
			let rest =
				message.strip_prefix("reset:").unwrap_or(message);
			let rest = rest.trim();

			let to = if rest.starts_with("moving to") {
				rest.strip_prefix("moving to")
					.map(|s| s.trim().to_string())
			} else {
				Some(rest.to_string())
			};

			Self::Reset { to }
		} else if message_lower.starts_with("rebase") {
			Self::Rebase
		} else if message_lower.starts_with("merge:") {
			Self::Merge
		} else if message_lower.starts_with("pull") {
			Self::Pull
		} else if message_lower.starts_with("clone:") {
			Self::Clone
		} else if message_lower.starts_with("branch:") {
			Self::Branch
		} else if message_lower.starts_with("amend:") {
			Self::Amend
		} else if message_lower.starts_with("cherry-pick:") {
			Self::CherryPick
		} else {
			Self::Unknown
		}
	}

	/// Get short display name for the action
	pub const fn short_name(&self) -> &'static str {
		match self {
			Self::Commit => "commit",
			Self::Checkout { .. } => "checkout",
			Self::Reset { .. } => "reset",
			Self::Rebase => "rebase",
			Self::Merge => "merge",
			Self::Pull => "pull",
			Self::Clone => "clone",
			Self::Branch => "branch",
			Self::Amend => "amend",
			Self::CherryPick => "cherry-pick",
			Self::Unknown => "other",
		}
	}

	/// Get the change description (from -> to for checkout, to for reset)
	pub fn change_description(&self) -> Option<String> {
		match self {
			Self::Checkout { from, to } => match (from, to) {
				(Some(f), Some(t)) => Some(format!("{f} -> {t}")),
				(Some(f), None) => Some(format!("{f} -> ?")),
				(None, Some(t)) => Some(format!("-> {t}")),
				(None, None) => None,
			},
			Self::Reset { to } => to.clone(),
			_ => None,
		}
	}
}

///
#[derive(Clone, Debug)]
pub struct ReflogEntry {
	/// ref name (e.g., "HEAD", "refs/heads/master", "HEAD -> origin/main")
	pub ref_name: String,
	/// old commit id (None for new refs)
	pub old_oid: Option<CommitId>,
	/// new commit id
	pub new_oid: CommitId,
	/// reflog message (the action, e.g., "commit: ...")
	pub message: String,
	/// parsed action type
	pub action: ReflogAction,
	/// timestamp (seconds since epoch)
	pub timestamp: i64,
	/// committer name
	pub committer: String,
	/// commit subject (the commit message's first line)
	pub commit_subject: String,
}

///
pub fn get_reflog(repo_path: &RepoPath) -> Result<Vec<ReflogEntry>> {
	scope_time!("get_reflog");

	let repo = repo(repo_path)?;
	let mut entries = Vec::new();

	// Get HEAD reflog using git2 native API
	if let Ok(reflog) = repo.reflog("HEAD") {
		for idx in 0..reflog.len() {
			if let Some(entry) = reflog.get(idx) {
				let new_oid = CommitId::new(entry.id_new());
				let old_oid = if entry.id_old().is_zero() {
					None
				} else {
					Some(CommitId::new(entry.id_old()))
				};

				let message = entry.message().unwrap_or_default();
				let action = ReflogAction::from_message(message);

				// Get committer info
				let committer = entry.committer();
				let committer_name = committer
					.name()
					.map(String::from)
					.unwrap_or_default();

				// Get commit subject by looking up the commit
				let commit_subject = repo
					.find_commit(entry.id_new())
					.ok()
					.and_then(|c| c.summary().map(String::from))
					.unwrap_or_default();

				entries.push(ReflogEntry {
					ref_name: "HEAD".to_string(),
					old_oid,
					new_oid,
					message: message.to_string(),
					action,
					timestamp: committer.when().seconds(),
					committer: committer_name,
					commit_subject,
				});
			}
		}
	}

	Ok(entries)
}
