mod daemon;
mod watcher;
mod remote;

use clap::{Parser, Subcommand};
use std::path::{PathBuf, Path};
use std::env;
use std::fs::{self, OpenOptions};
use std::io::{self, BufRead, Write};
use std::process::{Command, Stdio};
use anyhow::{Result, anyhow};
use chrono::Utc;
use daemon::{ConflictPolicy, DaemonTask, HistoryEvent, StateManager, TaskType, UndoEvent, generate_task_id};
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
        /// Human friendly task name
        #[arg(long)]
        name: Option<String>,
        /// Duplicate/conflict policy: skip, overwrite, rename, newest
        #[arg(long, default_value = "rename")]
        on_conflict: ConflictArg,
        /// Wait until files stop changing for this many seconds
        #[arg(long, default_value_t = 2)]
        stable_secs: u64,
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
        #[arg(long)]
        name: Option<String>,
        #[arg(long, default_value = "rename")]
        on_conflict: ConflictArg,
        #[arg(long, default_value_t = 2)]
        stable_secs: u64,
        #[arg(long)]
        include: Vec<String>,
        #[arg(long)]
        exclude: Vec<String>,
    },
    /// Sort files by glob rules like "*.pdf=C:\Docs"
    Sort {
        path: PathBuf,
        #[arg(long = "rule")]
        rules: Vec<String>,
        #[arg(long)]
        name: Option<String>,
        #[arg(long, default_value = "rename")]
        on_conflict: ConflictArg,
        #[arg(long, default_value_t = 2)]
        stable_secs: u64,
        #[arg(long)]
        dry_run: bool,
    },
    /// Clean/archive old files, optionally moving to local or remote destination
    Clean {
        path: PathBuf,
        #[arg(long)]
        older_than: String,
        #[arg(long)]
        move_to: Option<PathBuf>,
        #[arg(long)]
        scp: Option<String>,
        #[arg(long, default_value_t = 22)]
        port_scp: u16,
        #[arg(long)]
        rsync: bool,
        #[arg(long)]
        empty_dirs: bool,
        #[arg(long)]
        name: Option<String>,
        #[arg(long, default_value = "rename")]
        on_conflict: ConflictArg,
        #[arg(long)]
        dry_run: bool,
    },
    /// Show status of running daemons
    Status,
    /// Stop a daemon by PID, task id, or name
    Stop {
        target: String,
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
    /// Resume one paused task by id/name, or restart all enabled tasks when omitted
    ResumeTask {
        target: String,
    },
    /// Pause a task by id/name/PID
    Pause {
        target: String,
        #[arg(long = "for")]
        duration: Option<String>,
    },
    /// Diagnose common AFM problems
    Doctor,
    /// Fix safe state/config problems
    Repair,
    /// Show activity history
    History {
        #[arg(long)]
        task: Option<String>,
        #[arg(long)]
        failed: bool,
    },
    /// Export task config/state JSON
    Export,
    /// Import task config/state JSON
    Import {
        path: PathBuf,
    },
    /// Run a saved task once without creating a daemon
    RunOnce {
        target: String,
    },
    /// Undo the last reversible move operation
    Undo,
    /// Create common tasks interactively
    Init,
    /// Print available templates
    Template {
        name: Option<String>,
    },
    /// Send a native Windows test notification
    NotifyTest,
    /// Launch a lightweight Windows tray controller
    Tray,
    /// Show logs for a daemon PID
    Logs {
        target: String,
    },
}

#[derive(Clone, Copy, Debug)]
enum ConflictArg {
    Skip,
    Overwrite,
    Rename,
    Newest,
}

impl std::str::FromStr for ConflictArg {
    type Err = String;

    fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
        match value.to_ascii_lowercase().as_str() {
            "skip" => Ok(Self::Skip),
            "overwrite" => Ok(Self::Overwrite),
            "rename" => Ok(Self::Rename),
            "newest" => Ok(Self::Newest),
            _ => Err("expected skip, overwrite, rename, or newest".to_string()),
        }
    }
}

impl From<ConflictArg> for ConflictPolicy {
    fn from(value: ConflictArg) -> Self {
        match value {
            ConflictArg::Skip => ConflictPolicy::Skip,
            ConflictArg::Overwrite => ConflictPolicy::Overwrite,
            ConflictArg::Rename => ConflictPolicy::Rename,
            ConflictArg::Newest => ConflictPolicy::Newest,
        }
    }
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
        Commands::Unzip { path, no_delete, name, on_conflict, stable_secs } => {
            let abs_path = fs_canonicalize(&path)?;
            spawn_daemon(
                Commands::Unzip { path: abs_path, no_delete, name: name.clone(), on_conflict, stable_secs },
                &state_manager,
                name,
            )?;
        }
        Commands::Mirror { target_path, dest_path, scp, port_scp, rsync, name, on_conflict, stable_secs, include, exclude } => {
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
            spawn_daemon(
                Commands::Mirror { target_path: abs_target, dest_path: abs_dest, scp, port_scp, rsync, name: name.clone(), on_conflict, stable_secs, include, exclude },
                &state_manager,
                name,
            )?;
        }
        Commands::Sort { path, rules, name, on_conflict, stable_secs, dry_run } => {
            if rules.is_empty() {
                return Err(anyhow!("sort requires at least one --rule \"pattern=destination\""));
            }
            let abs_path = fs_canonicalize(&path)?;
            let task = TaskType::Sort {
                path: abs_path,
                rules,
                conflict: on_conflict.into(),
                stable_secs,
            };
            if dry_run {
                watcher::run_task_once(&task, true)?;
            } else {
                spawn_daemon_from_task(task, &state_manager, name)?;
            }
        }
        Commands::Clean { path, older_than, move_to, scp, port_scp, rsync, empty_dirs, name, on_conflict, dry_run } => {
            let abs_path = fs_canonicalize(&path)?;
            let days = parse_duration_days(&older_than)?;
            if scp.is_some() {
                ensure_mirror_requirements(true, rsync)?;
            }
            let task = TaskType::Clean {
                path: abs_path,
                older_than_days: days,
                move_to,
                remote: scp,
                port: Some(port_scp),
                use_rsync: rsync,
                empty_dirs,
                conflict: on_conflict.into(),
            };
            if dry_run {
                watcher::run_task_once(&task, true)?;
            } else {
                spawn_daemon_from_task(task, &state_manager, name)?;
            }
        }
        Commands::Status => {
            show_status(&state_manager)?;
        }
        Commands::Stop { target } => {
            let Some(task) = state_manager.find_task(&target)? else {
                println!("No daemon found for {}", target);
                return Ok(());
            };
            match kill_pid(task.pid) {
                Ok(true) => {
                    state_manager.remove_task(&target)?;
                    println!("Stopped daemon {} ({})", task.name.unwrap_or(task.id), task.pid);
                }
                Ok(false) => {
                    println!("No process found with PID {}, but removing from state anyway.", task.pid);
                    state_manager.remove_task(&target)?;
                }
                Err(e) => {
                    error!("Failed to stop daemon with PID {}: {}", task.pid, e);
                    if !state_manager.is_process_running(task.pid) {
                        state_manager.remove_task(&target)?;
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
                if task.enabled && !is_paused(&task) {
                    println!("Restarting task: {:?}", task.task);
                    let _ = kill_pid(task.pid);
                    state_manager.remove_task_by_pid(task.pid)?;
                    spawn_daemon_from_task(task.task, &state_manager, task.name)?;
                }
            }
        }
        Commands::ResumeTask { target } => {
            if state_manager.set_task_paused_until(&target, None)? {
                println!("Resumed task {}", target);
            } else {
                println!("No daemon found for {}", target);
            }
        }
        Commands::Pause { target, duration } => {
            let until = duration
                .as_deref()
                .map(parse_duration_seconds)
                .transpose()?
                .map(|seconds| Utc::now().timestamp() + seconds as i64);
            if state_manager.set_task_paused_until(&target, until)? {
                println!("Paused task {}", target);
            } else {
                println!("No daemon found for {}", target);
            }
        }
        Commands::Doctor => handle_doctor(&state_manager, false)?,
        Commands::Repair => handle_doctor(&state_manager, true)?,
        Commands::History { task, failed } => show_history(&state_manager, task, failed)?,
        Commands::Export => {
            println!("{}", serde_json::to_string_pretty(&state_manager.load_tasks()?)?);
        }
        Commands::Import { path } => {
            let content = fs::read_to_string(path)?;
            let tasks: Vec<DaemonTask> = serde_json::from_str(&content)?;
            state_manager.save_tasks(&tasks)?;
            println!("Imported {} task(s).", tasks.len());
        }
        Commands::RunOnce { target } => {
            let Some(task) = state_manager.find_task(&target)? else {
                return Err(anyhow!("No task found for {}", target));
            };
            watcher::run_task_once(&task.task, false)?;
        }
        Commands::Undo => handle_undo(&state_manager)?,
        Commands::Init => handle_init()?,
        Commands::Template { name } => show_template(name),
        Commands::NotifyTest => send_notification("AFM", "Notifications are working.")?,
        Commands::Tray => launch_tray(&state_manager)?,
        Commands::Logs { target } => {
            show_logs(&target, &state_manager)?;
        }
    }
    Ok(())
}

fn fs_canonicalize(path: &Path) -> Result<PathBuf> {
    Ok(std::fs::canonicalize(path)?)
}

fn spawn_daemon(command: Commands, state_manager: &StateManager, name: Option<String>) -> Result<()> {
    let task = match command {
        Commands::Unzip { path, no_delete, on_conflict, stable_secs, .. } => TaskType::Unzip {
            path,
            delete: !no_delete,
            conflict: on_conflict.into(),
            stable_secs,
        },
        Commands::Mirror { target_path, dest_path, scp, port_scp, rsync, on_conflict, stable_secs, include, exclude, .. } => TaskType::Mirror {
            src: target_path,
            dest: dest_path,
            remote: scp,
            port: Some(port_scp),
            use_rsync: rsync,
            conflict: on_conflict.into(),
            stable_secs,
            include,
            exclude,
        },
        _ => return Err(anyhow!("Cannot spawn daemon for this command")),
    };
    spawn_daemon_from_task(task, state_manager, name)
}

fn spawn_daemon_from_task(task: TaskType, state_manager: &StateManager, name: Option<String>) -> Result<()> {
    let exe = env::current_exe()?;
    let mut args = vec!["--daemon-internal".to_string()];
    
    match &task {
        TaskType::Unzip { path, delete, conflict, stable_secs } => {
            args.push("unzip".to_string());
            args.push(path.to_string_lossy().to_string());
            if !*delete {
                args.push("--no-delete".to_string());
            }
            args.push("--on-conflict".to_string());
            args.push(conflict_arg(*conflict).to_string());
            args.push("--stable-secs".to_string());
            args.push(stable_secs.to_string());
        }
        TaskType::Mirror { src, dest, remote, port, use_rsync, conflict, stable_secs, include, exclude } => {
            args.push("mirror".to_string());
            args.push(src.to_string_lossy().to_string());
            args.push(dest.to_string_lossy().to_string());
            if let Some(s) = remote {
                args.push("--scp".to_string());
                args.push(s.clone());
                args.push("--port-scp".to_string());
                args.push(port.unwrap_or(22).to_string());
            }
            if *use_rsync {
                args.push("--rsync".to_string());
            }
            args.push("--on-conflict".to_string());
            args.push(conflict_arg(*conflict).to_string());
            args.push("--stable-secs".to_string());
            args.push(stable_secs.to_string());
            for pattern in include {
                args.push("--include".to_string());
                args.push(pattern.clone());
            }
            for pattern in exclude {
                args.push("--exclude".to_string());
                args.push(pattern.clone());
            }
        }
        TaskType::Sort { path, rules, conflict, stable_secs } => {
            args.push("sort".to_string());
            args.push(path.to_string_lossy().to_string());
            for rule in rules {
                args.push("--rule".to_string());
                args.push(rule.clone());
            }
            args.push("--on-conflict".to_string());
            args.push(conflict_arg(*conflict).to_string());
            args.push("--stable-secs".to_string());
            args.push(stable_secs.to_string());
        }
        TaskType::Clean { path, older_than_days, move_to, remote, port, use_rsync, empty_dirs, conflict } => {
            args.push("clean".to_string());
            args.push(path.to_string_lossy().to_string());
            args.push("--older-than".to_string());
            args.push(format!("{}d", older_than_days));
            if let Some(dest) = move_to {
                args.push("--move-to".to_string());
                args.push(dest.to_string_lossy().to_string());
            }
            if let Some(remote) = remote {
                args.push("--scp".to_string());
                args.push(remote.clone());
                args.push("--port-scp".to_string());
                args.push(port.unwrap_or(22).to_string());
            }
            if *use_rsync {
                args.push("--rsync".to_string());
            }
            if *empty_dirs {
                args.push("--empty-dirs".to_string());
            }
            args.push("--on-conflict".to_string());
            args.push(conflict_arg(*conflict).to_string());
        }
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
    let id = generate_task_id();
    state_manager.add_task(DaemonTask { id: id.clone(), name, pid, enabled: true, paused_until: None, task })?;
    println!("Started daemon {} with PID {}", id, pid);
    Ok(())
}

fn conflict_arg(policy: ConflictPolicy) -> &'static str {
    match policy {
        ConflictPolicy::Skip => "skip",
        ConflictPolicy::Overwrite => "overwrite",
        ConflictPolicy::Rename => "rename",
        ConflictPolicy::Newest => "newest",
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
    if target.eq_ignore_ascii_case("all") {
        let tasks = state_manager.load_tasks()?;
        for task in &tasks {
            let _ = kill_pid(task.pid);
        }
        let count = state_manager.set_all_tasks_enabled(false)?;
        println!("Disabled {} daemon(s).", count);
    } else if let Some(task) = state_manager.find_task(&target)? {
        let _ = kill_pid(task.pid);
        if state_manager.set_task_enabled(&target, false)? {
            println!("Disabled daemon {}", target);
        }
    } else {
        println!("No daemon found for {}", target);
    }
    Ok(())
}

fn handle_delete(target: String, state_manager: &StateManager) -> Result<()> {
    if target.eq_ignore_ascii_case("all") {
        let tasks = state_manager.load_tasks()?;
        for task in &tasks {
            let _ = kill_pid(task.pid);
        }
        let count = state_manager.remove_all_tasks()?;
        let _ = handle_startup(true);
        println!("Deleted {} daemon(s).", count);
    } else if let Some(task) = state_manager.find_task(&target)? {
        let killed = match kill_pid(task.pid) {
            Ok(k) => k,
            Err(e) => {
                error!("Warning: Failed to kill process {}: {}", task.pid, e);
                !state_manager.is_process_running(task.pid)
            }
        };
        state_manager.remove_task(&target)?;
        if killed {
            println!("Deleted daemon {}", target);
        } else {
            println!("Removed daemon {} from state, but process might still be running.", target);
        }
    } else {
        println!("No daemon found for {}", target);
    }
    Ok(())
}

async fn run_daemon_logic(command: Commands) -> Result<()> {
    match command {
        Commands::Unzip { path, no_delete, on_conflict, stable_secs, .. } => {
            watcher::run_unzip_watcher(path, !no_delete, on_conflict.into(), stable_secs).await?;
        }
        Commands::Mirror { target_path, dest_path, scp, port_scp, rsync, on_conflict, stable_secs, include, exclude, .. } => {
            ensure_mirror_requirements(scp.is_some(), rsync)?;
            if let Some(remote) = scp {
                if rsync {
                    remote::run_remote_rsync_watcher(target_path, dest_path.to_string_lossy().to_string(), remote, port_scp).await?;
                } else {
                    remote::run_remote_mirror_watcher(target_path, dest_path.to_string_lossy().to_string(), remote, port_scp).await?;
                }
            } else {
                watcher::run_mirror_watcher(target_path, dest_path, on_conflict.into(), stable_secs, include, exclude).await?;
            }
        }
        Commands::Sort { path, rules, on_conflict, stable_secs, .. } => {
            watcher::run_sort_watcher(path, rules, on_conflict.into(), stable_secs).await?;
        }
        Commands::Clean { path, older_than, move_to, scp, port_scp, rsync, empty_dirs, on_conflict, .. } => {
            let task = TaskType::Clean {
                path,
                older_than_days: parse_duration_days(&older_than)?,
                move_to,
                remote: scp,
                port: Some(port_scp),
                use_rsync: rsync,
                empty_dirs,
                conflict: on_conflict.into(),
            };
            watcher::run_clean_loop(task).await?;
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

fn is_paused(task: &DaemonTask) -> bool {
    task.paused_until
        .map(|until| until > Utc::now().timestamp())
        .unwrap_or(false)
}

fn parse_duration_seconds(input: &str) -> Result<u64> {
    let trimmed = input.trim();
    let (num, mult) = if let Some(v) = trimmed.strip_suffix('h') {
        (v, 3600)
    } else if let Some(v) = trimmed.strip_suffix('m') {
        (v, 60)
    } else if let Some(v) = trimmed.strip_suffix('d') {
        (v, 86400)
    } else if let Some(v) = trimmed.strip_suffix('s') {
        (v, 1)
    } else {
        (trimmed, 1)
    };
    Ok(num.parse::<u64>()? * mult)
}

fn parse_duration_days(input: &str) -> Result<u64> {
    let seconds = parse_duration_seconds(input)?;
    Ok(std::cmp::max(1, seconds / 86400))
}

fn show_status(state_manager: &StateManager) -> Result<()> {
    let tasks = state_manager.load_tasks()?;
    if tasks.is_empty() {
        println!("No daemons found.");
        return Ok(());
    }

    use sysinfo::{Pid, System};
    let mut system = System::new_all();
    system.refresh_all();

    println!("{:<18} {:<16} {:<8} {:<10} {:<10} {}", "ID", "NAME", "PID", "STATE", "TYPE", "DETAILS");
    for task in tasks {
        let state = if is_paused(&task) {
            "PAUSED"
        } else if !task.enabled {
            "DISABLED"
        } else if system.process(Pid::from(task.pid as usize)).is_some() {
            "RUNNING"
        } else {
            "STOPPED"
        };
        println!(
            "{:<18} {:<16} {:<8} {:<10} {:<10} {}",
            short_id(&task.id),
            task.name.as_deref().unwrap_or("-"),
            task.pid,
            state,
            task_kind(&task.task),
            task_details(&task.task)
        );
    }
    Ok(())
}

fn short_id(id: &str) -> String {
    id.chars().take(18).collect()
}

fn task_kind(task: &TaskType) -> &'static str {
    match task {
        TaskType::Unzip { .. } => "unzip",
        TaskType::Mirror { .. } => "mirror",
        TaskType::Sort { .. } => "sort",
        TaskType::Clean { .. } => "clean",
    }
}

fn task_details(task: &TaskType) -> String {
    match task {
        TaskType::Unzip { path, delete, conflict, stable_secs } => {
            format!("{} delete={} conflict={:?} stable={}s", path.display(), delete, conflict, stable_secs)
        }
        TaskType::Mirror { src, dest, remote, use_rsync, conflict, .. } => {
            if let Some(remote) = remote {
                format!("{} -> {}:{} ({}, {:?})", src.display(), remote, dest.display(), if *use_rsync { "rsync" } else { "scp" }, conflict)
            } else {
                format!("{} -> {} ({:?})", src.display(), dest.display(), conflict)
            }
        }
        TaskType::Sort { path, rules, .. } => format!("{} rules={}", path.display(), rules.len()),
        TaskType::Clean { path, older_than_days, move_to, remote, .. } => {
            format!("{} older={}d move_to={:?} remote={:?}", path.display(), older_than_days, move_to, remote)
        }
    }
}

fn handle_doctor(state_manager: &StateManager, repair: bool) -> Result<()> {
    let mut tasks = state_manager.load_tasks()?;
    let mut issues = 0usize;
    let mut changed = false;

    for task in &mut tasks {
        if task.id.is_empty() {
            issues += 1;
            println!("missing id for PID {}", task.pid);
            if repair {
                task.id = generate_task_id();
                changed = true;
            }
        }
        if task.enabled && !state_manager.is_process_running(task.pid) {
            issues += 1;
            println!("stopped task: {} pid={}", task.name.as_deref().unwrap_or(&task.id), task.pid);
        }
        if !task_path_exists(&task.task) {
            issues += 1;
            println!("missing path: {}", task_details(&task.task));
        }
    }

    state_manager.ensure_log_dir()?;
    rotate_logs(state_manager, 10 * 1024 * 1024)?;

    if repair {
        let before = tasks.len();
        tasks.retain(|task| task.enabled || state_manager.is_process_running(task.pid) || task_path_exists(&task.task));
        if before != tasks.len() {
            changed = true;
        }
        if changed {
            state_manager.save_tasks(&tasks)?;
        }
        println!("Repair complete.");
    }

    if issues == 0 {
        println!("No issues found.");
    } else {
        println!("Found {} issue(s).", issues);
    }
    Ok(())
}

fn task_path_exists(task: &TaskType) -> bool {
    match task {
        TaskType::Unzip { path, .. } => path.exists(),
        TaskType::Mirror { src, dest, remote, .. } => src.exists() && (remote.is_some() || dest.exists()),
        TaskType::Sort { path, .. } => path.exists(),
        TaskType::Clean { path, .. } => path.exists(),
    }
}

fn rotate_logs(state_manager: &StateManager, max_size: u64) -> Result<()> {
    let dir = state_manager.config_dir().join("logs");
    if !dir.exists() {
        return Ok(());
    }
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        if path.extension().and_then(|s| s.to_str()) != Some("log") {
            continue;
        }
        if fs::metadata(&path)?.len() > max_size {
            let rotated = path.with_extension("log.1");
            let _ = fs::remove_file(&rotated);
            fs::rename(&path, rotated)?;
        }
    }
    Ok(())
}

fn show_history(state_manager: &StateManager, task: Option<String>, failed: bool) -> Result<()> {
    let path = state_manager.history_path();
    if !path.exists() {
        println!("No history yet.");
        return Ok(());
    }
    let file = fs::File::open(path)?;
    let mut lines: Vec<String> = io::BufReader::new(file)
        .lines()
        .collect::<std::result::Result<_, _>>()?;
    lines.reverse();
    for line in lines.into_iter().take(100) {
        let Ok(event) = serde_json::from_str::<HistoryEvent>(&line) else {
            continue;
        };
        if failed && event.level != "error" {
            continue;
        }
        if let Some(task_filter) = &task {
            if event.task.as_deref() != Some(task_filter.as_str()) {
                continue;
            }
        }
        println!("{} [{}] {}", event.ts, event.level, event.message);
    }
    Ok(())
}

fn handle_undo(state_manager: &StateManager) -> Result<()> {
    let path = state_manager.undo_path();
    if !path.exists() {
        println!("Nothing to undo.");
        return Ok(());
    }
    let mut lines: Vec<String> = io::BufReader::new(fs::File::open(&path)?)
        .lines()
        .collect::<std::result::Result<_, _>>()?;
    let Some(line) = lines.pop() else {
        println!("Nothing to undo.");
        return Ok(());
    };
    let event: UndoEvent = serde_json::from_str(&line)?;
    match event {
        UndoEvent::Move { src, dest } => {
            if dest.exists() {
                if let Some(parent) = src.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::rename(&dest, &src)?;
                println!("Moved {} back to {}", dest.display(), src.display());
            } else {
                println!("Cannot undo; {} no longer exists.", dest.display());
            }
        }
    }
    fs::write(path, lines.join("\n"))?;
    Ok(())
}

fn handle_init() -> Result<()> {
    println!("Use these starters:");
    println!("  afm unzip ~/Downloads --name downloads-zip");
    println!("  afm sort ~/Downloads --rule \"*.pdf=C:\\Users\\you\\Documents\" --name downloads-sort");
    println!("  afm clean ~/Downloads --older-than 30d --move-to D:\\Archive --name downloads-clean");
    Ok(())
}

fn show_template(name: Option<String>) {
    match name.as_deref() {
        Some("downloads-cleanup") => println!("afm clean ~/Downloads --older-than 30d --move-to ~/Archive --empty-dirs --name downloads-cleanup"),
        Some("zip-extractor") => println!("afm unzip ~/Downloads --name zip-extractor --on-conflict rename"),
        Some("photos-backup") => println!("afm mirror ~/Pictures D:\\PhotoBackup --name photos-backup"),
        Some("school-folder-sort") => println!("afm sort ~/Downloads --rule \"*.pdf=~/Documents/School\" --rule \"*.docx=~/Documents/School\" --name school-sort"),
        _ => {
            println!("downloads-cleanup");
            println!("zip-extractor");
            println!("photos-backup");
            println!("school-folder-sort");
        }
    }
}

fn send_notification(title: &str, body: &str) -> Result<()> {
    #[cfg(windows)]
    {
        let script = format!(
            "$wshell=New-Object -ComObject Wscript.Shell;$wshell.Popup('{}',3,'{}',64)|Out-Null",
            body.replace('\'', "''"),
            title.replace('\'', "''")
        );
        Command::new("powershell.exe")
            .args(["-NoProfile", "-WindowStyle", "Hidden", "-Command", &script])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
    }
    #[cfg(not(windows))]
    {
        let _ = (title, body);
    }
    Ok(())
}

fn launch_tray(state_manager: &StateManager) -> Result<()> {
    let exe = env::current_exe()?;
    let script_path = state_manager.config_dir().join("tray.ps1");
    let script = format!(
        r#"
Add-Type -AssemblyName System.Windows.Forms
$icon = New-Object System.Windows.Forms.NotifyIcon
$icon.Text = 'Auto File Manager'
$icon.Icon = [System.Drawing.SystemIcons]::Application
$menu = New-Object System.Windows.Forms.ContextMenuStrip
$status = $menu.Items.Add('Status')
$status.add_Click({{ Start-Process -FilePath '{}' -ArgumentList 'status' }})
$resume = $menu.Items.Add('Resume')
$resume.add_Click({{ Start-Process -FilePath '{}' -ArgumentList 'resume' -WindowStyle Hidden }})
$doctor = $menu.Items.Add('Doctor')
$doctor.add_Click({{ Start-Process -FilePath '{}' -ArgumentList 'doctor' }})
$exit = $menu.Items.Add('Exit Tray')
$exit.add_Click({{ $icon.Visible = $false; [System.Windows.Forms.Application]::Exit() }})
$icon.ContextMenuStrip = $menu
$icon.Visible = $true
[System.Windows.Forms.Application]::Run()
"#,
        exe.display(),
        exe.display(),
        exe.display()
    );
    fs::write(&script_path, script)?;
    Command::new("powershell.exe")
        .args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-WindowStyle", "Hidden", "-File"])
        .arg(&script_path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    println!("Launched AFM tray.");
    Ok(())
}

fn handle_startup(disable: bool) -> Result<()> {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let path = r"Software\Microsoft\Windows\CurrentVersion\Run";
    let (key, _) = hkcu.create_subkey(path)?;

    if disable {
        let _ = key.delete_value("autofilemgrd");
        let _ = std::fs::remove_file(startup_script_path()?);
        println!("Disabled auto-start.");
    } else {
        let exe = env::current_exe()?;
        let script_path = startup_script_path()?;
        let script = format!(
            "Set shell = CreateObject(\"WScript.Shell\")\r\nshell.Run \"cmd.exe /d /c \"\"{}\"\" resume\", 0, False\r\n",
            exe.to_string_lossy().replace('"', "\"\"")
        );
        std::fs::write(&script_path, script)?;

        let cmd = format!("wscript.exe //B //Nologo \"{}\"", script_path.to_string_lossy());
        key.set_value("autofilemgrd", &cmd)?;
        println!("Enabled auto-start at login.");
    }
    Ok(())
}

fn startup_script_path() -> Result<PathBuf> {
    let dir = dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("autofilemgrd");
    std::fs::create_dir_all(&dir)?;
    Ok(dir.join("startup.vbs"))
}

fn show_logs(target: &str, state_manager: &StateManager) -> Result<()> {
    let pid = if let Ok(pid) = target.parse::<u32>() {
        pid
    } else {
        state_manager
            .find_task(target)?
            .ok_or_else(|| anyhow!("No task found for {}", target))?
            .pid
    };
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
