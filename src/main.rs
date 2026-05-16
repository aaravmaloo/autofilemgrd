mod daemon;
mod watcher;
mod remote;

use clap::{Parser, Subcommand};
use std::path::{PathBuf, Path};
use std::env;
use std::fs::OpenOptions;
use std::io::{self, BufRead, Write};
use std::process::{Command, Stdio};
use anyhow::{Result, anyhow};
use daemon::{StateManager, DaemonTask, TaskType};
use tracing::{error, Level};
use tracing_subscriber::FmtSubscriber;
use winreg::enums::*;
use winreg::RegKey;
#[cfg(windows)]
use std::os::windows::process::CommandExt;

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x08000000;

#[derive(Parser)]
#[command(name = "afm")]
#[command(about = "Auto File Manager Daemon", long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Commands,

    /// Internal flag for daemon process
    #[arg(long, hide = true)]
    daemon_internal: bool,
}

#[derive(Subcommand)]
enum Commands {
    /// Start a watchdog for unzipping files in a folder
    Unzip {
        /// Path to the folder to monitor
        path: PathBuf,
        /// Do not delete the zip folder after unzipping
        #[arg(long)]
        no_delete: bool,
    },
    /// Start a mirror watchdog daemon
    Mirror {
        /// Source path to monitor
        target_path: PathBuf,
        /// Destination path to mirror to
        dest_path: PathBuf,
        /// Mirror to a remote machine via SCP (user@host)
        #[arg(long)]
        scp: Option<String>,
        /// Port for SCP connection
        #[arg(long, default_value_t = 22)]
        port_scp: u16,
        /// Use rsync transport for remote mirroring (Windows: runs via WSL)
        #[arg(long)]
        rsync: bool,
    },
    /// Show status of running daemons
    Status,
    /// Stop a daemon by PID
    Stop {
        pid: u32,
    },
    /// Disable a daemon by PID, or disable all daemons with "all"
    Disable {
        target: String,
    },
    /// Delete a daemon by PID, or delete all daemons with "all"
    Delete {
        target: String,
    },
    /// Enable auto-start on Windows startup
    Startup {
        #[arg(long)]
        disable: bool,
    },
    /// Resume all daemons from state (used for startup)
    Resume,
    /// Show logs for a daemon PID
    Logs {
        pid: u32,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let state_manager = StateManager::new()?;

    // Initialize logging
    if cli.daemon_internal {
        state_manager.ensure_log_dir()?;
        let pid = std::process::id();
        let log_file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(state_manager.log_path(pid))?;
        let subscriber = FmtSubscriber::builder()
            .with_max_level(Level::INFO)
            .with_writer(std::sync::Mutex::new(log_file))
            .finish();
        tracing::subscriber::set_global_default(subscriber).ok();
    } else {
        let subscriber = FmtSubscriber::builder()
            .with_max_level(Level::INFO)
            .finish();
        tracing::subscriber::set_global_default(subscriber).ok();
    }

    if cli.daemon_internal {
        run_daemon_logic(cli.command).await?;
    } else {
        handle_cli_command(cli.command, state_manager).await?;
    }

    Ok(())
}

async fn handle_cli_command(command: Commands, state_manager: StateManager) -> Result<()> {
    match command {
        Commands::Unzip { path, no_delete } => {
            let abs_path = fs_canonicalize(&path)?;
            spawn_daemon(Commands::Unzip { path: abs_path, no_delete }, &state_manager)?;
        }
        Commands::Mirror { target_path, dest_path, scp, port_scp, rsync } => {
            let abs_target = fs_canonicalize(&target_path)?;
            let abs_dest = if scp.is_none() { 
                let d = fs_canonicalize(&dest_path)?;
                if d.starts_with(&abs_target) {
                    return Err(anyhow!("Cannot mirror a folder into its own subdirectory: {:?}", d));
                }
                d
            } else { 
                dest_path 
            };
            ensure_mirror_requirements(scp.is_some(), rsync)?;
            spawn_daemon(Commands::Mirror { target_path: abs_target, dest_path: abs_dest, scp, port_scp, rsync }, &state_manager)?;
        }
        Commands::Status => {
            let tasks = state_manager.load_tasks()?;
            if tasks.is_empty() {
                println!("No daemons found.");
            } else {
                use sysinfo::{Pid, System};
                let mut system = System::new_all();
                system.refresh_all();

                println!("{:<10} {:<10} {:<10} {:<30}", "PID", "Type", "State", "Details");
                for task in tasks {
                    let state = if !task.enabled {
                        "DISABLED"
                    } else if system.process(Pid::from(task.pid as usize)).is_some() {
                        "RUNNING"
                    } else {
                        "STOPPED"
                    };
                    let details = match &task.task {
                        TaskType::Unzip { path, delete } => format!("Unzip: {:?} (del={})", path, delete),
                        TaskType::Mirror { src, dest, remote, use_rsync, .. } => {
                            if let Some(r) = remote {
                                let mode = if *use_rsync { "rsync" } else { "scp" };
                                format!("Mirror({}): {:?} -> {}@{}", mode, src, r, dest.display())
                            } else {
                                format!("Mirror(rsync): {:?} -> {:?}", src, dest)
                            }
                        }
                    };
                    println!("{:<10} {:<10} {:<10} {:<30}", task.pid, "Watcher", state, details);
                }
            }
        }
        Commands::Stop { pid } => {
            match kill_pid(pid) {
                Ok(true) => {
                    state_manager.remove_task(pid)?;
                    println!("Stopped daemon with PID {}", pid);
                }
                Ok(false) => {
                    println!("No process found with PID {}, but removing from state anyway.", pid);
                    state_manager.remove_task(pid)?;
                }
                Err(e) => {
                    error!("Failed to stop daemon with PID {}: {}", pid, e);
                    // If it failed to kill, maybe still remove from state if it's already gone?
                    if !state_manager.is_process_running(pid) {
                        state_manager.remove_task(pid)?;
                    }
                }
            }
        }
        Commands::Startup { disable } => {
            handle_startup(disable)?;
        }
        Commands::Disable { target } => {
            handle_disable(target, &state_manager)?;
        }
        Commands::Delete { target } => {
            handle_delete(target, &state_manager)?;
        }
        Commands::Resume => {
            let tasks = state_manager.load_tasks()?;
            for task in tasks {
                if task.enabled {
                    println!("Restarting task: {:?}", task.task);
                    state_manager.remove_task(task.pid)?;
                    let cmd = match task.task {
                        TaskType::Unzip { path, delete } => Commands::Unzip { path, no_delete: !delete },
                        TaskType::Mirror { src, dest, remote, port, use_rsync } => Commands::Mirror { 
                            target_path: src, 
                            dest_path: dest, 
                            scp: remote, 
                            port_scp: port.unwrap_or(22),
                            rsync: use_rsync,
                        },
                    };
                    spawn_daemon(cmd, &state_manager)?;
                }
            }
        }
        Commands::Logs { pid } => {
            show_logs(pid, &state_manager)?;
        }
    }
    Ok(())
}

fn fs_canonicalize(path: &Path) -> Result<PathBuf> {
    Ok(std::fs::canonicalize(path)?)
}

fn spawn_daemon(command: Commands, state_manager: &StateManager) -> Result<()> {
    let exe = env::current_exe()?;
    let mut args = vec!["--daemon-internal".to_string()];
    
    match &command {
        Commands::Unzip { path, no_delete } => {
            args.push("unzip".to_string());
            args.push(path.to_string_lossy().to_string());
            if *no_delete {
                args.push("--no-delete".to_string());
            }
        }
        Commands::Mirror { target_path, dest_path, scp, port_scp, rsync } => {
            args.push("mirror".to_string());
            args.push(target_path.to_string_lossy().to_string());
            args.push(dest_path.to_string_lossy().to_string());
            if let Some(s) = scp {
                args.push("--scp".to_string());
                args.push(s.clone());
                args.push("--port-scp".to_string());
                args.push(port_scp.to_string());
            }
            if *rsync {
                args.push("--rsync".to_string());
            }
        }
        _ => return Err(anyhow!("Cannot spawn daemon for this command")),
    }

    let mut cmd = Command::new(exe);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    let child = cmd.spawn()?;

    let pid = child.id();
    let task_type = match command {
        Commands::Unzip { path, no_delete } => TaskType::Unzip { path, delete: !no_delete },
        Commands::Mirror { target_path, dest_path, scp, port_scp, rsync } => TaskType::Mirror { src: target_path, dest: dest_path, remote: scp, port: Some(port_scp), use_rsync: rsync },
        _ => unreachable!(),
    };

    state_manager.add_task(DaemonTask { pid, enabled: true, task: task_type })?;
    println!("Started daemon with PID {}", pid);
    Ok(())
}

fn parse_target_pid(target: &str) -> Result<Option<u32>> {
    if target.eq_ignore_ascii_case("all") {
        Ok(None)
    } else {
        Ok(Some(target.parse::<u32>()?))
    }
}

fn is_afm_process(pid: u32, afm_exe: &Path) -> bool {
    use sysinfo::{Pid, System};
    let mut system = System::new();
    system.refresh_processes();

    let Some(process) = system.process(Pid::from(pid as usize)) else {
        return false;
    };

    let Some(process_exe) = process.exe() else {
        return false;
    };

    process_exe == afm_exe
}

fn kill_pid(pid: u32) -> Result<bool> {
    use sysinfo::{Pid, System};
    let afm_exe = env::current_exe()?;
    if !is_afm_process(pid, &afm_exe) {
        return Ok(false);
    }

    let mut system = System::new();
    system.refresh_processes();
    if let Some(process) = system.process(Pid::from(pid as usize)) {
        if process.kill() {
            // Wait a bit for the process to actually exit
            std::thread::sleep(std::time::Duration::from_millis(200));
            Ok(true)
        } else {
            Err(anyhow!("Failed to kill process {}", pid))
        }
    } else {
        Ok(false)
    }
}

fn handle_disable(target: String, state_manager: &StateManager) -> Result<()> {
    match parse_target_pid(&target)? {
        None => {
            let tasks = state_manager.load_tasks()?;
            for task in &tasks {
                let _ = kill_pid(task.pid);
            }
            let count = state_manager.set_all_tasks_enabled(false)?;
            println!("Disabled {} daemon(s).", count);
        }
        Some(pid) => {
            let _ = kill_pid(pid);
            if state_manager.set_task_enabled(pid, false)? {
                println!("Disabled daemon with PID {}", pid);
            } else {
                println!("No daemon found with PID {}", pid);
            }
        }
    }
    Ok(())
}

fn handle_delete(target: String, state_manager: &StateManager) -> Result<()> {
    match parse_target_pid(&target)? {
        None => {
            let tasks = state_manager.load_tasks()?;
            for task in &tasks {
                let _ = kill_pid(task.pid);
            }
            let count = state_manager.remove_all_tasks()?;
            let _ = handle_startup(true);
            println!("Deleted {} daemon(s).", count);
        }
        Some(pid) => {
            let killed = match kill_pid(pid) {
                Ok(k) => k,
                Err(e) => {
                    error!("Warning: Failed to kill process {}: {}", pid, e);
                    !state_manager.is_process_running(pid)
                }
            };
            
            let tasks = state_manager.load_tasks()?;
            if tasks.iter().any(|t| t.pid == pid) {
                state_manager.remove_task(pid)?;
                if killed {
                    println!("Deleted daemon with PID {}", pid);
                } else {
                    println!("Removed daemon with PID {} from state, but process might still be running.", pid);
                }
            } else {
                println!("No daemon found with PID {}", pid);
            }
        }
    }
    Ok(())
}

async fn run_daemon_logic(command: Commands) -> Result<()> {
    match command {
        Commands::Unzip { path, no_delete } => {
            watcher::run_unzip_watcher(path, !no_delete).await?;
        }
        Commands::Mirror { target_path, dest_path, scp, port_scp, rsync } => {
            ensure_mirror_requirements(scp.is_some(), rsync)?;
            if let Some(remote) = scp {
                if rsync {
                    remote::run_remote_rsync_watcher(target_path, dest_path.to_string_lossy().to_string(), remote, port_scp).await?;
                } else {
                    remote::run_remote_mirror_watcher(target_path, dest_path.to_string_lossy().to_string(), remote, port_scp).await?;
                }
            } else {
                watcher::run_mirror_watcher(target_path, dest_path).await?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn has_command(cmd: &str, args: &[&str]) -> bool {
    Command::new(cmd)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok()
}

#[cfg(windows)]
fn has_wsl_rsync() -> bool {
    Command::new("wsl")
        .args(["sh", "-lc", "command -v rsync >/dev/null 2>&1"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn ensure_mirror_requirements(is_remote: bool, use_rsync: bool) -> Result<()> {
    #[cfg(windows)]
    {
        if is_remote && !use_rsync {
            if !has_command("scp", &["-V"]) {
                return Err(anyhow!(
                    "scp is not available. Use '--rsync' to mirror via WSL rsync, or install OpenSSH scp."
                ));
            }
            return Ok(());
        }

        if !has_command("wsl", &["--status"]) {
            return Err(anyhow!(
                "WSL is not enabled. Enable WSL first, then install rsync inside WSL. \
                 For remote mirrors on Windows you can also skip '--rsync' to use scp."
            ));
        }
        if !has_wsl_rsync() {
            return Err(anyhow!(
                "rsync is not installed in WSL. Install it in your distro (e.g. 'sudo apt install rsync'). \
                 For remote mirrors on Windows you can use scp by omitting '--rsync'."
            ));
        }
        return Ok(());
    }

    #[cfg(not(windows))]
    {
        if use_rsync || !is_remote {
            if !has_command("rsync", &["--version"]) {
                return Err(anyhow!("rsync is not installed. Please install rsync and retry."));
            }
        } else if !has_command("scp", &["-V"]) {
            return Err(anyhow!("scp is not installed. Install OpenSSH scp or use '--rsync'."));
        }
        Ok(())
    }
}

fn handle_startup(disable: bool) -> Result<()> {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let path = r"Software\Microsoft\Windows\CurrentVersion\Run";
    let (key, _) = hkcu.create_subkey(path)?;

    if disable {
        key.delete_value("autofilemgrd")?;
        println!("Disabled auto-start.");
    } else {
        let exe = env::current_exe()?;
        let exe_escaped = exe.to_string_lossy().replace('\'', "''");
        let cmd = format!(
            "powershell.exe -NoProfile -WindowStyle Hidden -Command \"& '{}' resume\"",
            exe_escaped
        );
        key.set_value("autofilemgrd", &cmd)?;
        println!("Enabled auto-start at login.");
    }
    Ok(())
}

fn show_logs(pid: u32, state_manager: &StateManager) -> Result<()> {
    let path = state_manager.log_path(pid);
    if !path.exists() {
        return Err(anyhow!("No logs found for PID {} at {}", pid, path.display()));
    }

    let file = std::fs::File::open(&path)?;
    let reader = io::BufReader::new(file);
    let lines: Vec<String> = reader.lines().collect::<std::result::Result<_, _>>()?;
    let page_size = 25usize;
    let mut idx = 0usize;

    while idx < lines.len() {
        let end = usize::min(idx + page_size, lines.len());
        for line in &lines[idx..end] {
            println!("{}", line);
        }
        idx = end;
        if idx < lines.len() {
            print!("--More-- (Enter: next, q: quit) ");
            io::stdout().flush()?;
            let mut input = String::new();
            io::stdin().read_line(&mut input)?;
            let trimmed = input.trim().to_ascii_lowercase();
            if trimmed == "q" {
                println!("(END)");
                return Ok(());
            }
        }
    }
    println!("(END)");
    Ok(())
}
