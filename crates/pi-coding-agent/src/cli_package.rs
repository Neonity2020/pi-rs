//! `pi package` — install, remove, update, and list resource packages —
//! port of the package surface of
//! `pi/packages/coding-agent/src/package-manager-cli.ts` (`handlePackageCommand`
//! minus the self-update machinery, which belongs to the npm launcher; the
//! `pi config` selector is a declared deferral).

use crate::core::package_manager::{PackageManager, parse_source};
use serde_json::json;

const APP_NAME: &str = "pi-rs";

fn help(command: Option<&str>) -> String {
	match command {
		Some("install") => format!(
			"Usage: {APP_NAME} package install <source> [-l]\n\n\
Install a package and persist it in settings.json. A package provides\n\
extensions (TS/JS), skills (SKILL.md), prompt templates (md), and themes\n\
(json) from an npm registry package (`npm:<name>[@version]`), a git URL, or\n\
a local directory.\n\nOptions:\n  -l, --local    Install for the current project (requires a trusted project)"
		),
		Some("remove") => format!(
			"Usage: {APP_NAME} package remove <source> [-l]\n\n\
Remove a package's install artifacts and its settings entry.\n\nOptions:\n  -l, --local    Remove from the current project's package config"
		),
		Some("update") => format!(
			"Usage: {APP_NAME} package update [source]\n\n\
Update configured packages (unpinned npm specs and git checkouts).\n\
With a source, only the matching package is updated."
		),
		Some("list") => format!(
			"Usage: {APP_NAME} package list\n\n\
List configured packages with their scope and install path."
		),
		_ => format!(
			"Usage:\n  {APP_NAME} package install <source> [-l]\n  {APP_NAME} package remove <source> [-l]\n  {APP_NAME} package list\n  {APP_NAME} package update [source]\n\n\
Install, remove, update, and list resource packages (extensions, skills,\n\
prompt templates, themes) from npm, git, or local directories."
		),
	}
}

/// Run `pi package <args>` and return the exit code — upstream
/// `handlePackageCommand` (returns `false` when `args` is not a package
/// command, matching the upstream contract).
pub async fn run_package_command(args: &[String]) -> Option<i32> {
	let (command, rest) = match args.first().map(String::as_str) {
		Some("install") | Some("remove") | Some("list") | Some("update") => {
			(args.first().unwrap().as_str(), &args[1..])
		}
		Some("help") | Some("--help") | Some("-h") => {
			println!("{}", help(None));
			return Some(0);
		}
		_ => return None,
	};

	let mut local = false;
	let mut approve = false;
	let mut positional: Vec<String> = Vec::new();
	let mut invalid: Option<String> = None;
	for arg in rest {
		match arg.as_str() {
			"-l" | "--local" => local = true,
			"--approve" => approve = true,
			"--help" | "-h" => {
				println!("{}", help(Some(command)));
				return Some(0);
			}
			other if other.starts_with('-') => {
				invalid = Some(other.to_string());
			}
			other => positional.push(other.to_string()),
		}
	}
	if let Some(option) = invalid {
		eprintln!("Unknown option {option} for \"{command}\".");
		eprintln!("Use \"{APP_NAME} package help\" for usage.");
		return Some(1);
	}

	let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
	let agent_dir = crate::config::agent_dir();
	let project_trusted = if approve {
		true
	} else {
		crate::core::trust::is_trusted(&cwd)
	};
	let manager = PackageManager::new(&cwd, &agent_dir, project_trusted);

	match command {
		"install" => {
			let Some(source) = positional.first() else {
				eprintln!("Missing install source.");
				eprintln!("Usage: {APP_NAME} package install <source> [-l]");
				return Some(1);
			};
			if !positional[1..].is_empty() {
				eprintln!("Usage: {APP_NAME} package install <source> [-l]");
				return Some(1);
			}
			if local && !project_trusted {
				eprintln!("Project is not trusted. Use --approve to modify local package config.");
				return Some(1);
			}
			if let Err(error) = manager.install(source, local).await {
				eprintln!("Error: {error}");
				return Some(1);
			}
			if let Err(error) = manager.add_source_to_settings(source, local) {
				eprintln!("Error: {error}");
				return Some(1);
			}
			println!("Installed {source}");
			Some(0)
		}
		"remove" => {
			let Some(source) = positional.first() else {
				eprintln!("Missing remove source.");
				eprintln!("Usage: {APP_NAME} package remove <source> [-l]");
				return Some(1);
			};
			if !positional[1..].is_empty() {
				eprintln!("Usage: {APP_NAME} package remove <source> [-l]");
				return Some(1);
			}
			if local && !project_trusted {
				eprintln!("Project is not trusted. Use --approve to modify local package config.");
				return Some(1);
			}
			if let Err(error) = manager.remove(source, local).await {
				eprintln!("Error: {error}");
				return Some(1);
			}
			match manager.remove_source_from_settings(source, local) {
				Ok(true) => {
					println!("Removed {source}");
					Some(0)
				}
				Ok(false) => {
					eprintln!("No matching package found for {source}");
					Some(1)
				}
				Err(error) => {
					eprintln!("Error: {error}");
					Some(1)
				}
			}
		}
		"list" => {
			for package in manager.list_configured() {
				let identity = match parse_source(&package.source) {
					crate::core::package_manager::ParsedSource::Npm { name, .. } => name,
					crate::core::package_manager::ParsedSource::Git { host, path, .. } => {
						format!("{host}/{path}")
					}
					crate::core::package_manager::ParsedSource::Local { path } => path,
				};
				let installed = match &package.installed_path {
					Some(path) => path.display().to_string(),
					None => "not installed".to_string(),
				};
				println!(
					"{}",
					json!({
						"source": package.source,
						"name": identity,
						"scope": package.scope,
						"installed": package.installed_path.is_some(),
						"path": installed,
					})
				);
			}
			Some(0)
		}
		"update" => {
			if !positional.is_empty() && positional.len() > 1 {
				eprintln!("Usage: {APP_NAME} package update [source]");
				return Some(1);
			}
			if let Err(error) = manager.update(positional.first().map(String::as_str)).await {
				eprintln!("Error: {error}");
				return Some(1);
			}
			Some(0)
		}
		_ => Some(1),
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn args(list: &[&str]) -> Vec<String> {
		list.iter().map(|arg| (*arg).to_string()).collect()
	}

	fn run(list: &[&str]) -> Option<i32> {
		let argv = args(list);
		futures::executor::block_on(run_package_command(&argv))
	}

	#[test]
	fn non_package_commands_return_none() {
		assert_eq!(run(&["auth"]), None);
		assert_eq!(run(&["mcp"]), None);
		assert_eq!(run(&[]), None);
	}

	#[test]
	fn help_and_unknown_options_follow_the_upstream_shape() {
		assert_eq!(run(&["--help"]), Some(0));
		assert_eq!(run(&["install", "--wat"]), Some(1));
		assert_eq!(run(&["install"]), Some(1));
	}
}
