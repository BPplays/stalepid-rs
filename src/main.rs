use anyhow::{Context, Result};
use clap::Parser;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use sysinfo::{Pid, System};
use tokio::fs as tokio_fs;
use tokio::task::JoinSet;

#[derive(Parser, Debug)]
#[command(author, version, about = "Check for and remove stale process ID files")]
struct Args {
    /// List of PID files to check. Format: <path> or <path>=<process_name>
    #[arg(short = 'p', num_args = 1..)]
    pid_files: Option<Vec<String>>,

    /// Directory to scan for PID files
    #[arg(short = 'd')]
    directory: Option<PathBuf>,

    /// File extension to use when scanning a directory
    #[arg(short = 'e', default_value = ".pid")]
    extension: String,

    /// Process name to validate against.
    /// Acts as fallback for -p files without explicit names and as the filter for -d.
    process_name: Option<String>,
}

async fn is_pid_stale(sys: &System, pid_path: &Path, expected_name: Option<&str>) -> Result<bool> {
    let content = tokio_fs::read_to_string(pid_path).await?;
    let pid_str = content.trim();
    
    if pid_str.is_empty() {
        return Ok(true);
    }

    let pid_val = pid_str.parse::<usize>().map_err(|_| {
        anyhow::anyhow!("PID file {:?} contains invalid PID: {}", pid_path, pid_str)
    })?;

    let pid = Pid::from(pid_val);

    if let Some(process) = sys.process(pid) {
        if let Some(name) = expected_name {
            if process.name() != name {
                return Ok(true);
            }
        }
        return Ok(false);
    }

    Ok(true)
}

async fn handle_pid_file(sys: Arc<System>, path: PathBuf, expected_name: Option<String>) -> Result<()> {
    if is_pid_stale(&sys, &path, expected_name.as_deref()).await? {
        tokio_fs::remove_file(&path).await
            .with_context(|| format!("Failed to remove stale pid file {:?}", path))?;
        println!("Removed stale pid file: {:?}", path);
    }
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let mut sys_raw = System::new_all();
    sys_raw.refresh_all();
    let sys = Arc::new(sys_raw);
    let global_name = args.process_name.clone();

    let mut tasks = JoinSet::new();

    if let Some(ref pids) = args.pid_files {
        for p in pids {
            let parts: Vec<&str> = p.splitn(2, '=').collect();
            let file = PathBuf::from(parts[0]);
            let name = parts.get(1).map(|s| s.to_string()).or(global_name.clone());
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

    if args.pid_files.is_none() && args.directory.is_none() {
        anyhow::bail!("Either -p or -d must be specified");
    }

    while let Some(res) = tasks.join_next().await {
        if let Err(e) = res {
            eprintln!("Task panicked: {}", e);
        } else if let Ok(Err(e)) = res {
            eprintln!("Error processing file: {}", e);
        }
    }

    Ok(())
}

