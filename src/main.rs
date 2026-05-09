mod daemon;
mod watcher;
mod remote;

use clap::{Parser, Subcommand};
use std::path::{PathBuf, Path};
use std::env;
use std::process::{Command, Stdio};
use anyhow::{Result, anyhow};
use daemon::{StateManager, DaemonTask, TaskType};
use tracing::Level;
use tracing_subscriber::FmtSubscriber;
use winreg::enums::*;
use winreg::RegKey;

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
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    // Initialize logging
    let subscriber = FmtSubscriber::builder()
        .with_max_level(Level::INFO)
        .finish();
    tracing::subscriber::set_global_default(subscriber).ok();

    let state_manager = StateManager::new()?;

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
        Commands::Mirror { target_path, dest_path, scp, port_scp } => {
            let abs_target = fs_canonicalize(&target_path)?;
            let abs_dest = if scp.is_none() { fs_canonicalize(&dest_path)? } else { dest_path };
            spawn_daemon(Commands::Mirror { target_path: abs_target, dest_path: abs_dest, scp, port_scp }, &state_manager)?;
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
                        TaskType::Mirror { src, dest, remote, .. } => {
                            if let Some(r) = remote {
                                format!("Mirror: {:?} -> {}@{}", src, r, dest.display())
                            } else {
                                format!("Mirror: {:?} -> {:?}", src, dest)
                            }
                        }
                    };
                    println!("{:<10} {:<10} {:<10} {:<30}", task.pid, "Watcher", state, details);
                }
            }
        }
        Commands::Stop { pid } => {
            use sysinfo::{Pid, System};
            let mut system = System::new_all();
            system.refresh_all();
            if let Some(process) = system.process(Pid::from(pid as usize)) {
                process.kill();
                state_manager.remove_task(pid)?;
                println!("Stopped daemon with PID {}", pid);
            } else {
                println!("No process found with PID {}", pid);
                state_manager.remove_task(pid)?;
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
                        TaskType::Mirror { src, dest, remote, port } => Commands::Mirror { 
                            target_path: src, 
                            dest_path: dest, 
                            scp: remote, 
                            port_scp: port.unwrap_or(22) 
                        },
                    };
                    spawn_daemon(cmd, &state_manager)?;
                }
            }
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
        Commands::Mirror { target_path, dest_path, scp, port_scp } => {
            args.push("mirror".to_string());
            args.push(target_path.to_string_lossy().to_string());
            args.push(dest_path.to_string_lossy().to_string());
            if let Some(s) = scp {
                args.push("--scp".to_string());
                args.push(s.clone());
                args.push("--port-scp".to_string());
                args.push(port_scp.to_string());
            }
        }
        _ => return Err(anyhow!("Cannot spawn daemon for this command")),
    }

    let child = Command::new(exe)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;

    let pid = child.id();
    let task_type = match command {
        Commands::Unzip { path, no_delete } => TaskType::Unzip { path, delete: !no_delete },
        Commands::Mirror { target_path, dest_path, scp, port_scp } => TaskType::Mirror { src: target_path, dest: dest_path, remote: scp, port: Some(port_scp) },
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

fn kill_pid(pid: u32) {
    use sysinfo::{Pid, System};
    let mut system = System::new_all();
    system.refresh_all();
    if let Some(process) = system.process(Pid::from(pid as usize)) {
        process.kill();
    }
}

fn handle_disable(target: String, state_manager: &StateManager) -> Result<()> {
    match parse_target_pid(&target)? {
        None => {
            let tasks = state_manager.load_tasks()?;
            for task in &tasks {
                kill_pid(task.pid);
            }
            let count = state_manager.set_all_tasks_enabled(false)?;
            println!("Disabled {} daemon(s).", count);
        }
        Some(pid) => {
            kill_pid(pid);
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
                kill_pid(task.pid);
            }
            let count = state_manager.remove_all_tasks()?;
            println!("Deleted {} daemon(s).", count);
        }
        Some(pid) => {
            kill_pid(pid);
            let tasks = state_manager.load_tasks()?;
            if tasks.iter().any(|t| t.pid == pid) {
                state_manager.remove_task(pid)?;
                println!("Deleted daemon with PID {}", pid);
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
        Commands::Mirror { target_path, dest_path, scp, port_scp } => {
            if let Some(remote) = scp {
                remote::run_remote_mirror_watcher(target_path, dest_path.to_string_lossy().to_string(), remote, port_scp).await?;
            } else {
                watcher::run_mirror_watcher(target_path, dest_path).await?;
            }
        }
        _ => {}
    }
    Ok(())
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
        let cmd = format!("\"{}\" resume", exe.to_string_lossy());
        key.set_value("autofilemgrd", &cmd)?;
        println!("Enabled auto-start at login.");
    }
    Ok(())
}
