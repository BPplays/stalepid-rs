use anyhow::{Context, Result};
use clap::Parser;
use std::path::{Path, PathBuf};
use sysinfo::{Pid, System};
use tokio::fs as tokio_fs;

#[derive(Parser, Debug)]
#[command(author, version, about = "Check for and remove stale process ID files")]
struct Args {
    /// List of PID files to check
    #[arg(short = 'p', num_args = 1..)]
    pid_files: Option<Vec<PathBuf>>,

    /// Directory to scan for PID files
    #[arg(short = 'd')]
    directory: Option<PathBuf>,

    /// File extension to use when scanning a directory
    #[arg(short = 'e', default_value = ".pid")]
    extension: String,

    /// Process name to validate against. If omitted, any process with the PID prevents deletion.
    process_name: Option<String>,
}

async fn is_pid_stale(sys: &mut System, pid_path: &Path, expected_name: Option<&str>) -> Result<bool> {
    let content = tokio_fs::read_to_string(pid_path).await?;
    let pid_str = content.trim();
    
    if pid_str.is_empty() {
        return Ok(true);
    }

    let pid_val = pid_str.parse::<usize>().map_err(|_| {
        anyhow::anyhow!("PID file {:?} contains invalid PID: {}", pid_path, pid_str)
    })?;

    let pid = Pid::from(pid_val);
    sys.refresh_all();

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

async fn handle_pid_file(sys: &mut System, path: PathBuf, expected_name: Option<&str>) -> Result<()> {
    if is_pid_stale(sys, &path, expected_name).await? {
        tokio_fs::remove_file(&path).await
            .with_context(|| format!("Failed to remove stale pid file {:?}", path))?;
        println!("Removed stale pid file: {:?}", path);
    }
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let mut sys = System::new_all();
    let expected_name = args.process_name.as_deref();

    let mut files_to_check = Vec::new();

    if let Some(ref pids) = args.pid_files {
        files_to_check.extend(pids.clone());
    }

    if let Some(ref dir) = args.directory {
        let mut entries = tokio_fs::read_dir(dir).await?;
        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            if path.is_file() {
                let path_str = path.to_string_lossy();
                if path_str.ends_with(&args.extension) {
                    files_to_check.push(path);
                }
            }
        }
    }

    if files_to_check.is_empty() {
        if args.pid_files.is_none() && args.directory.is_none() {
            anyhow::bail!("Either -p or -d must be specified");
        }
        return Ok(());
    }

    for file in files_to_check {
        if let Err(e) = handle_pid_file(&mut sys, file, expected_name).await {
            eprintln!("Error processing file: {}", e);
        }
    }

    Ok(())
}
