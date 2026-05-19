use notify::{Config, RecommendedWatcher, RecursiveMode, Watcher, Event, EventKind};
use std::path::{Path, PathBuf};
use std::fs;
use std::process::Command;
use zip::ZipArchive;
use anyhow::{Result, anyhow};
use chrono::Utc;
use glob::Pattern;
use tracing::{info, error, debug};
use std::io;
use crate::daemon::{ConflictPolicy, HistoryEvent, StateManager, TaskType, UndoEvent};

pub async fn run_unzip_watcher(path: PathBuf, delete: bool, conflict: ConflictPolicy, stable_secs: u64) -> Result<()> {
    let (tx, mut rx) = tokio::sync::mpsc::channel(1);

    let mut watcher = RecommendedWatcher::new(move |res: notify::Result<Event>| {
        if let Ok(event) = res {
            if matches!(event.kind, EventKind::Create(_) | EventKind::Modify(_)) {
                for path in event.paths {
                    if path.extension().and_then(|s| s.to_str()) == Some("zip") {
                        let _ = tx.blocking_send(path);
                    }
                }
            }
        }
    }, Config::default())?;

    watcher.watch(&path, RecursiveMode::NonRecursive)?;
    info!("Watching for zip files in {:?}", path);

    while let Some(zip_path) = rx.recv().await {
        if !path.exists() {
            info!("Watched directory {:?} deleted. Stopping unzip watcher.", path);
            break;
        }
        info!("Found zip: {:?}", zip_path);
        if let Err(e) = wait_until_stable(&zip_path, stable_secs).and_then(|_| unzip_and_handle(&zip_path, delete, conflict)) {
            error!("Error unzipping {:?}: {}", zip_path, e);
            let _ = append_history("error", format!("unzip failed {}: {}", zip_path.display(), e));
        }
    }

    Ok(())
}

fn unzip_and_handle(zip_path: &Path, delete: bool, conflict: ConflictPolicy) -> Result<()> {
    let file = fs::File::open(zip_path)?;
    let mut archive = ZipArchive::new(file)?;
    
    let dest_dir = zip_path.with_extension("");
    if !dest_dir.exists() {
        fs::create_dir_all(&dest_dir)?;
    }

    for i in 0..archive.len() {
        let mut file = archive.by_index(i)?;
        let outpath = match file.enclosed_name() {
            Some(path) => dest_dir.join(path),
            None => continue,
        };

        if file.name().ends_with('/') {
            fs::create_dir_all(&outpath)?;
        } else {
            if let Some(p) = outpath.parent() {
                if !p.exists() {
                    fs::create_dir_all(p)?;
                }
            }
            let outpath = resolve_conflict(&outpath, conflict)?;
            let mut outfile = fs::File::create(&outpath)?;
            io::copy(&mut file, &mut outfile)?;
            let _ = append_history("info", format!("extracted {}", outpath.display()));
        }
    }

    if delete {
        fs::remove_file(zip_path)?;
        info!("Deleted zip: {:?}", zip_path);
    }

    Ok(())
}

pub async fn run_mirror_watcher(src: PathBuf, dest: PathBuf, conflict: ConflictPolicy, stable_secs: u64, include: Vec<String>, exclude: Vec<String>) -> Result<()> {
    // Initial sync
    sync_dir_with_rsync(&src, &dest, conflict, &include, &exclude)?;

    let (tx, mut rx) = tokio::sync::mpsc::channel(1);

    let mut watcher = RecommendedWatcher::new(move |res: notify::Result<Event>| {
        if let Ok(event) = res {
            let _ = tx.blocking_send(event);
        }
    }, Config::default())?;

    watcher.watch(&src, RecursiveMode::Recursive)?;
    info!("Mirroring {:?} to {:?}", src, dest);

    while let Some(event) = rx.recv().await {
        if !src.exists() {
            info!("Source directory {:?} deleted. Stopping mirror watcher.", src);
            break;
        }
        debug!("Mirror event: {:?}", event);
        if let Err(e) = handle_mirror_event(event, &src, &dest, conflict, stable_secs, &include, &exclude) {
            error!("Mirror error: {}", e);
            let _ = append_history("error", format!("mirror failed {} -> {}: {}", src.display(), dest.display(), e));
        }
    }

    Ok(())
}

fn sync_dir_with_rsync(src: &Path, dest: &Path, conflict: ConflictPolicy, include: &[String], exclude: &[String]) -> Result<()> {
    if !dest.exists() {
        fs::create_dir_all(dest)?;
    }

    let src_arg = format!("{}/", src.to_string_lossy().replace('\\', "/"));
    let dest_arg = format!("{}/", dest.to_string_lossy().replace('\\', "/"));
    let mut cmd = Command::new("rsync");
    cmd.arg("-a");
    if conflict != ConflictPolicy::Skip {
        cmd.arg("--delete");
    }
    if conflict == ConflictPolicy::Skip {
        cmd.arg("--ignore-existing");
    }
    for pattern in include {
        cmd.arg("--include").arg(pattern);
    }
    for pattern in exclude {
        cmd.arg("--exclude").arg(pattern);
    }
    cmd.arg(&src_arg).arg(&dest_arg);
    let status = cmd.status()?;
    if !status.success() {
        return Err(anyhow!("rsync failed with status {}", status));
    }
    Ok(())
}

fn handle_mirror_event(event: Event, src_root: &Path, dest_root: &Path, conflict: ConflictPolicy, stable_secs: u64, include: &[String], exclude: &[String]) -> Result<()> {
    for path in event.paths {
        if path.is_file() {
            let _ = wait_until_stable(&path, stable_secs);
        }
    }
    // For correctness, re-run rsync on each filesystem event.
    sync_dir_with_rsync(src_root, dest_root, conflict, include, exclude)?;
    Ok(())
}

pub async fn run_sort_watcher(path: PathBuf, rules: Vec<String>, conflict: ConflictPolicy, stable_secs: u64) -> Result<()> {
    let (tx, mut rx) = tokio::sync::mpsc::channel(1);
    let mut watcher = RecommendedWatcher::new(move |res: notify::Result<Event>| {
        if let Ok(event) = res {
            let _ = tx.blocking_send(event);
        }
    }, Config::default())?;

    watcher.watch(&path, RecursiveMode::NonRecursive)?;
    info!("Sorting files in {:?}", path);

    while let Some(event) = rx.recv().await {
        for path in event.paths {
            if path.is_file() {
                if let Err(e) = wait_until_stable(&path, stable_secs).and_then(|_| sort_file(&path, &rules, conflict, false)) {
                    error!("Sort error: {}", e);
                    let _ = append_history("error", format!("sort failed {}: {}", path.display(), e));
                }
            }
        }
    }
    Ok(())
}

pub async fn run_clean_loop(task: TaskType) -> Result<()> {
    loop {
        run_task_once(&task, false)?;
        tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
    }
}

pub fn run_task_once(task: &TaskType, dry_run: bool) -> Result<()> {
    match task {
        TaskType::Unzip { path, delete, conflict, stable_secs } => {
            for entry in fs::read_dir(path)? {
                let path = entry?.path();
                if path.extension().and_then(|s| s.to_str()) == Some("zip") {
                    wait_until_stable(&path, *stable_secs)?;
                    if dry_run {
                        println!("would unzip {}", path.display());
                    } else {
                        unzip_and_handle(&path, *delete, *conflict)?;
                    }
                }
            }
        }
        TaskType::Mirror { src, dest, conflict, include, exclude, .. } => {
            if dry_run {
                println!("would mirror {} -> {}", src.display(), dest.display());
            } else {
                sync_dir_with_rsync(src, dest, *conflict, include, exclude)?;
            }
        }
        TaskType::Sort { path, rules, conflict, stable_secs } => {
            for entry in fs::read_dir(path)? {
                let path = entry?.path();
                if path.is_file() {
                    wait_until_stable(&path, *stable_secs)?;
                    sort_file(&path, rules, *conflict, dry_run)?;
                }
            }
        }
        TaskType::Clean { path, older_than_days, move_to, remote, port, use_rsync, empty_dirs, conflict } => {
            clean_path(path, *older_than_days, move_to.as_ref(), remote.as_ref(), port.unwrap_or(22), *use_rsync, *empty_dirs, *conflict, dry_run)?;
        }
    }
    Ok(())
}

fn sort_file(path: &Path, rules: &[String], conflict: ConflictPolicy, dry_run: bool) -> Result<()> {
    let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
        return Ok(());
    };
    for rule in rules {
        let Some((pattern, dest)) = rule.split_once('=') else {
            return Err(anyhow!("invalid rule {}, expected pattern=destination", rule));
        };
        if Pattern::new(pattern)?.matches(name) {
            let dest_dir = PathBuf::from(dest);
            let dest_path = resolve_conflict(&dest_dir.join(name), conflict)?;
            if dry_run {
                println!("would move {} -> {}", path.display(), dest_path.display());
            } else {
                fs::create_dir_all(&dest_dir)?;
                move_with_conflict(path, &dest_path, conflict)?;
                append_undo(path, &dest_path)?;
                let _ = append_history("info", format!("moved {} -> {}", path.display(), dest_path.display()));
            }
            break;
        }
    }
    Ok(())
}

fn clean_path(path: &Path, older_than_days: u64, move_to: Option<&PathBuf>, remote: Option<&String>, port: u16, use_rsync: bool, empty_dirs: bool, conflict: ConflictPolicy, dry_run: bool) -> Result<()> {
    let cutoff = std::time::SystemTime::now() - std::time::Duration::from_secs(older_than_days * 86400);
    for entry in walkdir::WalkDir::new(path).min_depth(1).contents_first(true) {
        let entry = entry?;
        let entry_path = entry.path();
        if entry_path.is_dir() {
            if empty_dirs && fs::read_dir(entry_path)?.next().is_none() {
                if dry_run {
                    println!("would remove empty dir {}", entry_path.display());
                } else {
                    let _ = fs::remove_dir(entry_path);
                }
            }
            continue;
        }
        if fs::metadata(entry_path)?.modified()? > cutoff {
            continue;
        }
        if let Some(remote) = remote {
            if let Some(dest) = move_to {
                if dry_run {
                    println!("would move {} -> {}:{}", entry_path.display(), remote, dest.display());
                } else if use_rsync {
                    remote_move_with_rsync(entry_path, dest, remote, port)?;
                    fs::remove_file(entry_path)?;
                } else {
                    return Err(anyhow!("remote clean --move-to currently requires --rsync"));
                }
            }
        } else if let Some(dest_dir) = move_to {
            let relative = entry_path.strip_prefix(path).unwrap_or(entry_path);
            let dest = resolve_conflict(&dest_dir.join(relative), conflict)?;
            if dry_run {
                println!("would move {} -> {}", entry_path.display(), dest.display());
            } else {
                if let Some(parent) = dest.parent() {
                    fs::create_dir_all(parent)?;
                }
                move_with_conflict(entry_path, &dest, conflict)?;
                append_undo(entry_path, &dest)?;
            }
        } else if dry_run {
            println!("would delete {}", entry_path.display());
        } else {
            fs::remove_file(entry_path)?;
        }
    }
    Ok(())
}

fn wait_until_stable(path: &Path, stable_secs: u64) -> Result<()> {
    if stable_secs == 0 || !path.exists() {
        return Ok(());
    }
    let mut last = fs::metadata(path)?.len();
    loop {
        std::thread::sleep(std::time::Duration::from_secs(stable_secs));
        if !path.exists() {
            return Ok(());
        }
        let current = fs::metadata(path)?.len();
        if current == last {
            return Ok(());
        }
        last = current;
    }
}

fn resolve_conflict(path: &Path, policy: ConflictPolicy) -> Result<PathBuf> {
    if !path.exists() {
        return Ok(path.to_path_buf());
    }
    match policy {
        ConflictPolicy::Skip => Ok(path.to_path_buf()),
        ConflictPolicy::Overwrite => Ok(path.to_path_buf()),
        ConflictPolicy::Newest => Ok(path.to_path_buf()),
        ConflictPolicy::Rename => {
            let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("file");
            let ext = path.extension().and_then(|s| s.to_str()).map(|s| format!(".{}", s)).unwrap_or_default();
            for idx in 1..10_000 {
                let candidate = path.with_file_name(format!("{} ({}){}", stem, idx, ext));
                if !candidate.exists() {
                    return Ok(candidate);
                }
            }
            Err(anyhow!("could not find available duplicate name for {}", path.display()))
        }
    }
}

fn move_with_conflict(src: &Path, dest: &Path, policy: ConflictPolicy) -> Result<()> {
    if dest.exists() {
        match policy {
            ConflictPolicy::Skip => return Ok(()),
            ConflictPolicy::Overwrite => fs::remove_file(dest)?,
            ConflictPolicy::Rename => {}
            ConflictPolicy::Newest => {
                let src_modified = fs::metadata(src)?.modified()?;
                let dest_modified = fs::metadata(dest)?.modified()?;
                if src_modified <= dest_modified {
                    return Ok(());
                }
                fs::remove_file(dest)?;
            }
        }
    }
    fs::rename(src, dest)?;
    Ok(())
}

fn remote_move_with_rsync(path: &Path, remote_dest: &Path, remote: &str, port: u16) -> Result<()> {
    #[cfg(windows)]
    {
        let src = windows_path_to_wsl(path)?;
        let dest = remote_dest.to_string_lossy().replace('\\', "/");
        let cmd = format!(
            "rsync -az -e 'ssh -p {}' '{}' '{}:{}/'",
            port,
            single_quote_for_shell(&src),
            single_quote_for_shell(remote),
            single_quote_for_shell(&dest)
        );
        let status = Command::new("wsl").args(["sh", "-lc", &cmd]).status()?;
        if !status.success() {
            return Err(anyhow!("remote rsync move failed with status {}", status));
        }
        return Ok(());
    }
    #[cfg(not(windows))]
    {
    let dest = remote_dest.to_string_lossy();
    let status = Command::new("rsync")
        .args(["-az", "-e", &format!("ssh -p {}", port)])
        .arg(path)
        .arg(format!("{}:{}/", remote, dest))
        .status()?;
    if !status.success() {
        return Err(anyhow!("remote rsync move failed with status {}", status));
    }
    Ok(())
    }
}

#[cfg(windows)]
fn windows_path_to_wsl(path: &Path) -> Result<String> {
    let mut raw = path.to_string_lossy().to_string();
    if let Some(stripped) = raw.strip_prefix(r"\\?\") {
        raw = stripped.to_string();
    }
    let raw = raw.replace('\\', "/");
    if raw.len() >= 3 && raw.as_bytes()[1] == b':' && raw.as_bytes()[2] == b'/' {
        let drive = raw.chars().next().unwrap_or('c').to_ascii_lowercase();
        Ok(format!("/mnt/{}/{}", drive, &raw[3..]))
    } else {
        Err(anyhow!("Unsupported Windows path for WSL rsync: {}", raw))
    }
}

#[cfg(windows)]
fn single_quote_for_shell(input: &str) -> String {
    input.replace('\'', "'\"'\"'")
}

fn append_history(level: &str, message: String) -> Result<()> {
    let state = StateManager::new()?;
    state.append_history(&HistoryEvent {
        ts: Utc::now().timestamp(),
        task: None,
        level: level.to_string(),
        message,
    })
}

fn append_undo(src: &Path, dest: &Path) -> Result<()> {
    let state = StateManager::new()?;
    state.append_undo(&UndoEvent::Move { src: src.to_path_buf(), dest: dest.to_path_buf() })
}
