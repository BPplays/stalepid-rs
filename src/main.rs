use anyhow::{Context, Result};
use clap::Parser;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;
use sysinfo::{Pid, System};
use tokio::fs as tokio_fs;
use tokio::task::JoinSet;
use quoted_string::to_content;
use std::borrow::Cow;

#[derive(Debug, Clone)]
struct PidProc {
    file: PathBuf,
    name: Option<String>,
}

impl FromStr for PidProc {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        // Handle brace format: {path,name} or {path}
        if s.starts_with('{') && s.ends_with('}') {
            let inner = &s[1..s.len() - 1];
            let parts = split_respecting_quotes(inner);

            if parts.is_empty() {
                anyhow::bail!("PID file path cannot be empty in brace format");
            }

            let file = parse_quoted_part(parts.get(0).unwrap_or(&String::new()))?;
            let name = parts.get(1).and_then(|p| parse_quoted_part(p).ok());

            return Ok(PidProc {
                file: PathBuf::from(file),
                name,
            });
        }

        // Handle path=name format
        if let Some((path, name)) = s.split_once('=') {
            return Ok(PidProc {
                file: PathBuf::from(parse_quoted_part(path)?),
                name: Some(parse_quoted_part(name)?),
            });
        }

        // Handle plain path format
        Ok(PidProc {
            file: PathBuf::from(parse_quoted_part(s)?),
            name: None,
        })
    }
}

fn parse_quoted_part(s: &str) -> Result<String, anyhow::Error> {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        anyhow::bail!("Empty value provided");
    }

    let qs = quoted_string::parse(trimmed)
        .map_err(|e| anyhow::anyhow!("Invalid quoting in part '{}': {}", trimmed, e))?;

    let con = quoted_string::to_content(qs).unwrap_or(Cow::Owned(""));
    Ok(con.into_owned())
}

fn split_respecting_quotes(s: &str) -> Vec<String> {
    let mut result = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    let mut quote_char = ' ';

    let chars: Vec<char> = s.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if (c == '"' || c == '\'') && (!in_quotes || quote_char == c) {
            if in_quotes {
                in_quotes = false;
            } else {
                in_quotes = true;
                quote_char = c;
            }
            current.push(c);
        } else if c == ',' && !in_quotes {
            result.push(current.trim().to_string());
            current = String::new();
        } else {
            current.push(c);
        }
        i += 1;
    }
    result.push(current.trim().to_string());
    result
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
