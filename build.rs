use std::process::Command;

fn get_version() -> String {
	if let Ok(commit) = std::env::var("BUILD_GIT_COMMIT_ID") {
		return commit[..7].to_string();
	}

	let describe = Command::new("git")
		.arg("describe")
		.arg("--tags")
		.arg("--always")
		.arg("--dirty")
		.output();

	let version = match describe {
		Ok(output) => {
			let raw = String::from_utf8_lossy(&output.stdout);
			let line = raw.lines().next().unwrap_or("").trim();
			if line.is_empty() {
				return "unknown".to_string();
			}
			line.trim_start_matches('v').to_string()
		}
		Err(e) => {
			panic!("Can not get git describe: {e}");
		}
	};

	version
}

fn main() {
	let build_name = if std::env::var("GITUI_RELEASE").is_ok() {
		env!("CARGO_PKG_VERSION").to_string()
	} else {
		get_version()
	};

	println!("cargo:warning=buildname '{build_name}'");
	println!("cargo:rustc-env=GITUI_BUILD_NAME={build_name}");

	println!("cargo:rerun-if-changed=build.rs");
}
