use anyhow::{Context, Result};
use clap::Parser;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;
use sysinfo::{Pid, System};
use tokio::fs as tokio_fs;
use tokio::task::JoinSet;
use tracing::{info, warn, error};
use tracing_subscriber::{fmt, prelude::*};
use logroller::{LogRollerBuilder, Rotation, RotationSize};
use walkdir::WalkDir;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PidProc {
	file: PathBuf,
	name: Option<String>,
}

impl FromStr for PidProc {
	type Err = anyhow::Error;

	fn from_str(s: &str) -> Result<Self, Self::Err> {
		if s.starts_with('{') && s.ends_with('}') {
			let inner = &s[1..s.len() - 1];
			let parts: Vec<&str> = inner.split(',').collect();
			let mut file = String::new();
			let mut name = None;

			for (i, part) in parts.iter().enumerate() {
				let trimmed = part.trim().trim_matches('\'').trim_matches('"');
				if i == 0 {
					file = trimmed.to_string();
				} else if i == 1 {
					name = Some(trimmed.to_string());
				}
			}
			if file.is_empty() {
				anyhow::bail!("PID file path cannot be empty in brace format");
			}
			return Ok(PidProc {
				file: PathBuf::from(file),
				name,
			});
		}

		if let Some((path, name)) = s.split_once('=') {
			return Ok(PidProc {
				file: PathBuf::from(path),
				name: Some(name.to_string()),
			});
		}

		Ok(PidProc {
			file: PathBuf::from(s),
			name: None,
		})
	}
}

#[derive(Parser, Debug)]
#[command(author, version, about = "Check for and remove stale process ID files")]
struct Args {
	/// List of PID files to check. Format: <path>, <path>=<process_name>, or {<path>,<process_name>}
	#[arg(short = 'p', num_args = 1..)]
	pid_files: Option<Vec<PidProc>>,

	/// Directory to scan for PID files
	#[arg(short = 'd')]
	directory: Option<PathBuf>,

	/// File extension to use when scanning a directory
	#[arg(short = 'e', default_value = ".pid")]
	extension: String,

	/// Process name to validate against.
	/// Acts as fallback for -p files without explicit names and as the filter for -d.
	process_name: Option<String>,

	/// Path to a directory containing YAML files with PID/Process name pairs.
	#[arg(long)]
	pidpair_dir: Option<PathBuf>,

	/// Path to the log file. If omitted, logs to terminal.
	#[arg(long)]
	log_path: Option<PathBuf>,

	/// Maximum size of the log file in bytes before rotation.
	#[arg(long)]
	max_log_size: Option<u64>,
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

		if let Some(size) = args.max_log_size {
			builder = builder.rotation(Rotation::SizeBased(RotationSize::Bytes(size)));
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

async fn is_pid_stale(sys: &System, pid_path: &Path, expected_name: Option<&str>) -> Result<bool> {
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

	if let Some(process) = sys.process(pid) {
		if let Some(name) = expected_name {
			let actual_name = process.name().to_string_lossy();
			if actual_name != name {
				warn!(path = %path_str, pid = %pid_val, process_name = %actual_name, expected_name = %name, "Process name mismatch, marking as stale");
				return Ok(true);
			}
		}
		return Ok(false);
	}

	info!(path = %path_str, pid = pid_val, "No process found with PID, marking as stale");
	Ok(true)
}

async fn handle_pid_file(sys: Arc<System>, path: PathBuf, expected_name: Option<String>) -> Result<()> {
	let path_str = path.to_string_lossy();
	if is_pid_stale(&sys, &path, expected_name.as_deref()).await? {
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
	let global_name = args.process_name.clone();

	let mut tasks = JoinSet::new();

	if let Some(ref pids) = args.pid_files {
		for pid_proc in pids {
			let name = pid_proc.name.clone().or(global_name.clone());
			let file = pid_proc.file.clone();
			let sys_clone = Arc::clone(&sys);
			tasks.spawn(async move {
				handle_pid_file(sys_clone, file, name).await
			});
		}
	}

	if let Some(ref dir) = args.pidpair_dir {
		let pids = load_pid_pairs_from_dir(dir).with_context(|| format!("Failed to load pid pairs from {:?}", dir))?;
		for pid_proc in pids {
			let name = pid_proc.name.clone().or(global_name.clone());
			let file = pid_proc.file.clone();
			let sys_clone = Arc::clone(&sys);
			tasks.spawn(async move {
				handle_pid_file(sys_clone, file, name).await
			});
		}
	}

	if let Some(ref dir) = args.directory {
		let mut entries = tokio_fs::read_dir(dir).await?;
		while let Some(entry) = entries.next_entry().await? {
			let path = entry.path();
			if path.is_file() {
				let path_str = path.to_string_lossy();
				if path_str.ends_with(&args.extension) {
					let name = global_name.clone();
					let sys_clone = Arc::clone(&sys);
					tasks.spawn(async move {
						handle_pid_file(sys_clone, path, name).await
					});
				}
			}
		}
	}

	if args.pid_files.is_none() && args.directory.is_none() && args.pidpair_dir.is_none() {
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


