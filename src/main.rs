use anyhow::{Context, Result, anyhow};
use clap::Parser;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;
use sysinfo::{Pid, System};
use tokio::fs as tokio_fs;
use tokio::task::JoinSet;
use tracing::{info, warn, error};
// use tracing_subscriber::{fmt, prelude::*};
use logroller::{LogRollerBuilder, Rotation, RotationSize};
use walkdir::WalkDir;
use futures::future::BoxFuture;
use futures::FutureExt;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct PidProc {
	file: PathBuf,
	name: String,
	#[serde(default)]
	daemon_recurse_limit: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DaemonName {
	name: String,
	pid: Pid,
}

fn between_chars<'a>(s: &'a str, left: char, right: char) -> Option<&'a str> {
	let left_len = left.len_utf8();
	// Find the start index after the left character
	if let Some(start) = s.find(left) {
		let start = start + left_len;
		if start < s.len() {
			let rest = &s[start..];
			// Find the right character in the remaining string
			if let Some(end) = rest.find(right) {
				return Some(&rest[..end]);
			}
		}
	}
	None
}

#[derive(Debug)]
enum ParseError {
	MissingQuotedString,
}

fn parse_quoted(i: &str) -> Result<&str> {
	between_chars(i, '"', '"')
		.ok_or(ParseError::MissingQuotedString)
		.map_err(|e| anyhow::anyhow!("{:?}", e))
}

impl FromStr for PidProc {
	type Err = anyhow::Error;

	fn from_str(s: &str) -> Result<Self, Self::Err> {

		if s.starts_with('{') && s.ends_with('}') {
			let inner = &s[1..s.len() - 1];
			let parts: Vec<&str> = inner.split(',').collect();
			let mut file: Option<String> = None;
			let mut name: Option<String> = None;

			for (i, part) in parts.iter().enumerate() {
				let trimmed = part.trim();
				let trimmed = parse_quoted(trimmed)?;
				if i == 0 {
					file = Some(trimmed.to_string());
				} else if i == 1 {
					name = Some(trimmed.to_string());
				}
			}

			let name = name.ok_or_else(|| anyhow::anyhow!("name was none"))?;
			let file = file.ok_or_else(|| anyhow::anyhow!("file was none"))?;

			if file.is_empty() {
				anyhow::bail!("PID file path cannot be empty in brace format");
			}
			return Ok(PidProc {
				file: PathBuf::from(file),
				name: name,
				daemon_recurse_limit: 0,
			});
		}

		if s.starts_with('{') {
			return Err(anyhow::anyhow!("opening {{ but no closing"))
		}

		if s.ends_with('}') {
			return Err(anyhow::anyhow!("closing }} but no opening"))
		}

		if let Some((path, name)) = s.split_once('=') {
			if path.is_empty() {
				anyhow::bail!("PID file path cannot be empty");
			}
			if name.is_empty() {
				return Err(anyhow::anyhow!("name was empty"));
			}

			return Ok(PidProc {
				file: PathBuf::from(path),
				name: name.to_string(),
				daemon_recurse_limit: 0,
			});
		}


		return Err(anyhow::anyhow!("name was never found"));
	}
}

#[derive(Parser, Debug)]
#[command(author, version, about = "Check for and remove stale process ID files")]
struct Args {
	/// List of PID files to check. Format: <path>, <path>=<process_name>, or {<path>,<process_name>}
	#[arg(short = 'p', num_args = 1..)]
	pid_files: Option<Vec<PidProc>>,

	// /// Directory to scan for PID files
	// #[arg(short = 'd')]
	// directory: Option<PathBuf>,

	// /// File extension to use when scanning a directory
	// #[arg(short = 'e', default_value = ".pid")]
	// extension: String,

	// /// Process name to validate against.
	// /// Acts as fallback for -p files without explicit names and as the filter for -d.
	// process_name: Option<String>,

	/// Path to a directory containing YAML files with PID/Process name pairs.
	#[arg(long)]
	pidpair_dir: Option<PathBuf>,

	/// Path to the log file. If omitted, logs to terminal.
	#[arg(long)]
	log_path: Option<PathBuf>,

	/// Maximum size of the log file in megabytes before rotation.
	#[arg(long, default_value = "10")]
	max_log_size_mb: Option<u64>,
}

fn init_logging(args: &Args) -> Result<()> {
	if let Some(path) = &args.log_path {
		let parent = path.parent().unwrap_or_else(|| Path::new("."));
		let filename = path.file_name()
			.and_then(|s| s.to_str())
			.ok_or_else(|| anyhow::anyhow!("Invalid log file path"))?;

		let mut builder = LogRollerBuilder::new(
			parent.to_str().ok_or_else(|| anyhow::anyhow!("Invalid log directory"))?,
			filename
		);

		if let Some(size) = args.max_log_size_mb {
			builder = builder.rotation(Rotation::SizeBased(RotationSize::MB(size)));
		} else {
			builder = builder.rotation(Rotation::SizeBased(RotationSize::MB(10)));
		}

		let appender = builder.build()?;
		let (non_blocking, _guard) = tracing_appender::non_blocking(appender);

		tracing_subscriber::fmt()
			.with_writer(non_blocking)
			.init();

		Box::leak(Box::new(_guard));
	} else {
		tracing_subscriber::fmt::init();
	}
	Ok(())
}

async fn match_daemon_name(
	name: &str,
) -> Result<DaemonName> {
	let s = name
		.strip_prefix("daemon: ")
		.ok_or_else(|| anyhow!("missing daemon prefix"))?;

	let (procname, rest) = s
		.split_once('[')
		.ok_or_else(|| anyhow!("missing pid start"))?;

	let pid_str = rest
		.strip_suffix(']')
		.ok_or_else(|| anyhow!("missing pid end"))?;

	let pidu32: u32 = pid_str.parse()?;
	let pid: Pid = Pid::from_u32(pidu32);
	let d = DaemonName{
		name: procname.to_string(),
		pid: pid,
	};

	Ok(d)
}

async fn is_pid_path_stale(
	sys: &System,
	pid_path: &Path,
	name: &str,
	daemon_recurse_limit: u64,
) -> Result<bool> {
	let path_str = pid_path.to_string_lossy();
	let content = tokio_fs::read_to_string(pid_path).await?;
	let pid_str = content.trim();

	if pid_str.is_empty() {
		warn!(path = %path_str, "PID file is empty, marking as stale");
		return Ok(true);
	}

	let pid_val = pid_str.parse::<usize>().map_err(|_| {
		anyhow::anyhow!("PID file {:?} contains invalid PID: {}", pid_path, pid_str)
	})?;

	let pid = Pid::from(pid_val);

	return is_pid_stale(sys, &pid, name, daemon_recurse_limit).await;
}

fn is_pid_stale<'a>(
	sys: &'a System,
	pid: &'a Pid,
	name: &'a str,
	daemon_recurse_limit: u64,
) -> BoxFuture<'a, Result<bool>> {
	async move {
	if let Some(process) = sys.process(*pid) {
		let actual_name = process.name().to_string_lossy();
		let cmd = process.cmd();
		let exe = process.exe();

		let exe_str = exe
			.map(|p| p.to_string_lossy().into_owned())
			.unwrap_or_else(|| "<none>".to_string());

		let cmd_str = cmd
			.iter()
			.map(|s| s.to_string_lossy())
			.collect::<Vec<_>>()
			.join(" ");

		if daemon_recurse_limit > 0 {
			let daemon_name = match_daemon_name(&actual_name).await;
			match daemon_name {
				Ok(dn) => {
					info!(
						child_name = %dn.name,
						child_pid = %dn.pid,
						"daemon child found"
					);

					return is_pid_stale(
						sys,
						&dn.pid,
						name,
						daemon_recurse_limit - 1,
					)
					.await;
				}
				Err(err) => {
					info!(err = %err, name = %actual_name, "daemon name not found");
				}
			}
		}

		if actual_name != name {
			warn!(
				pid = %pid,
				process_name = %actual_name,
				expected_name = %name,
				exe = %exe_str,
				cmd = %cmd_str,
				"Process name mismatch, marking as stale"
			);
			return Ok(false);
		}

		return Ok(false);
	}

	info!(pid = %pid, "No process found with PID, marking as stale");
	Ok(true)
	}
	.boxed()
}

async fn handle_pid_file(sys: Arc<System>, pid_proc: &PidProc) -> Result<()> {
	let path = pid_proc.file.clone();
	let path_str = pid_proc.file.to_string_lossy();

	if is_pid_path_stale(
		&sys,
		&path,
		&pid_proc.name,
		pid_proc.daemon_recurse_limit,
	).await? {
		if !path.is_file() {
			return Err(anyhow::anyhow!("path isn't file"))
		}
		tokio_fs::remove_file(&path).await
			.with_context(|| format!("Failed to remove stale pid file {:?}", path))?;
		info!(path = %path_str, "Removed stale pid file");
	}
	Ok(())
}

fn load_pid_pairs_from_dir(dir: &Path) -> Result<Vec<PidProc>> {
	let mut entries: Vec<_> = WalkDir::new(dir)
		.into_iter()
		.filter_map(|e| e.ok())
		.filter(|e| {
			let path = e.path();
			path.is_file() &&
			(path.extension().map_or(false, |ext| ext == "yml" || ext == "yaml"))
		})
		.collect();

	entries.sort_by(|a, b| a.path().cmp(b.path()));

	let mut all_procs = Vec::new();
	for entry in entries {
		let content = std::fs::read_to_string(entry.path())
			.with_context(|| format!("Failed to read config file {:?}", entry.path()))?;
		let procs: Vec<PidProc> = serde_yaml::from_str(&content)
			.with_context(|| format!("Failed to parse YAML in file {:?}", entry.path()))?;
		all_procs.extend(procs);
	}

	Ok(all_procs)
}

#[tokio::main]
async fn main() -> Result<()> {
	let args = Args::parse();

	if let Err(e) = init_logging(&args) {
		eprintln!("Failed to initialize logging: {}", e);
		std::process::exit(1);
	}

	let mut sys_raw = System::new_all();
	sys_raw.refresh_all();
	let sys = Arc::new(sys_raw);
	// let global_name = args.process_name.clone();

	let mut tasks = JoinSet::new();

	if let Some(ref pids) = args.pid_files {
		for pid_proc in pids {
			let sys_clone = Arc::clone(&sys);
			let pc = pid_proc.clone();
			tasks.spawn(async move {
				handle_pid_file(sys_clone, &pc).await
			});
		}
	}

	if let Some(ref dir) = args.pidpair_dir {
		let pids = load_pid_pairs_from_dir(dir).with_context(|| format!("Failed to load pid pairs from {:?}", dir))?;
		for pid_proc in pids {
			let sys_clone = Arc::clone(&sys);
			tasks.spawn(async move {
				handle_pid_file(sys_clone, &pid_proc).await
			});
		}
	}

	// if let Some(ref dir) = args.directory {
	// 	let mut entries = tokio_fs::read_dir(dir).await?;
	// 	while let Some(entry) = entries.next_entry().await? {
	// 		let path = entry.path();
	// 		if path.is_file() {
	// 			let path_str = path.to_string_lossy();
	// 			if path_str.ends_with(&args.extension) {
	// 				let name = global_name.clone();
	// 				let sys_clone = Arc::clone(&sys);
	// 				tasks.spawn(async move {
	// 					handle_pid_file(sys_clone, path, name).await
	// 				});
	// 			}
	// 		}
	// 	}
	// }

	// if args.pid_files.is_none() && args.directory.is_none() && args.pidpair_dir.is_none() {
	if args.pid_files.is_none() && args.pidpair_dir.is_none() {
		error!("Neither -p, -d, nor --pidpair-dir specified");
		anyhow::bail!("Either -p, -d, or --pidpair-dir must be specified");
	}

	while let Some(res) = tasks.join_next().await {
		if let Err(e) = res {
			error!(error = %e, "Task panicked");
		} else if let Ok(Err(e)) = res {
			error!(error = %e, "Error processing file");
		}
	}

	Ok(())
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn test_between_chars_basic() {
		assert_eq!(between_chars("hello", 'h', 'o'), Some("ell"));
		assert_eq!(between_chars("hello", 'e', 'l'), Some(""));
	}

	#[test]
	fn test_between_chars_unicode() {
		assert_eq!(between_chars("🦀hello🦀", '🦀', '🦀'), Some("hello"));
	}

	#[test]
	fn test_between_chars_empty() {
		assert_eq!(between_chars("()", '(', ')'), Some(""));
	}

	#[test]
	fn test_between_chars_missing() {
		assert_eq!(between_chars("hello", 'z', 'o'), None);
		assert_eq!(between_chars("hello", 'h', 'z'), None);
	}

	#[test]
	fn test_parse_quoted_success() {
		assert_eq!(parse_quoted("\"test\"").unwrap(), "test");
		assert_eq!(parse_quoted("\"\"").unwrap(), "");
	}

	#[test]
	fn test_parse_quoted_failure() {
		assert!(parse_quoted("test\"").is_err());
		assert!(parse_quoted("\"test").is_err());
		assert!(parse_quoted("test").is_err());
	}

	// Test the actual directory loading functionality with mock data
	#[test]
	fn test_load_pid_pairs_from_dir() {
		// Create a temporary directory structure for testing
		let test_dir = "test_data";

		// Test that the function doesn't panic when called
		let result = load_pid_pairs_from_dir(Path::new(test_dir));
		println!("{:#?}", result.as_ref().unwrap());

		// This test makes sure the function compiles and doesn't crash
		// with the test data directory that exists
		assert!(result.is_ok());
	}


	#[test]
	fn test_match_daemon_name() {
		let rt = tokio::runtime::Runtime::new().unwrap();
		rt.block_on(async {
			assert!(match_daemon_name("fdjsf").await.is_err());

			let daemons = vec![
				DaemonName {
					name: "prg".to_string(),
					pid: Pid::from(1111),
				},
				DaemonName {
					name: "日本program".to_string(),
					pid: Pid::from(22229),
				},
				DaemonName {
					name: "sshd".to_string(),
					pid: Pid::from(3333999),
				},
			];

			for d in &daemons {
				println!("daemon: {}[{}]", d.name, d.pid);
				assert!(
					match_daemon_name(&format!(
							"daemon: {}[{}]",
							d.name,
							d.pid,
					)).await.unwrap() == d.clone()
				);
			}
		});
	}

}
