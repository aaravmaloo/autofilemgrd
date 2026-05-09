use notify::{Config, RecommendedWatcher, RecursiveMode, Watcher, Event, EventKind};
use std::path::{Path, PathBuf};
use std::fs;
use zip::ZipArchive;
use anyhow::{Result, anyhow};
use tracing::{info, error, debug};
use std::io;

pub async fn run_unzip_watcher(path: PathBuf, delete: bool) -> Result<()> {
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
        info!("Found zip: {:?}", zip_path);
        if let Err(e) = unzip_and_handle(&zip_path, delete) {
            error!("Error unzipping {:?}: {}", zip_path, e);
        }
    }

    Ok(())
}

fn unzip_and_handle(zip_path: &Path, delete: bool) -> Result<()> {
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
            let mut outfile = fs::File::create(&outpath)?;
            io::copy(&mut file, &mut outfile)?;
        }
    }

    if delete {
        fs::remove_file(zip_path)?;
        info!("Deleted zip: {:?}", zip_path);
    }

    Ok(())
}

pub async fn run_mirror_watcher(src: PathBuf, dest: PathBuf) -> Result<()> {
    // Initial sync
    sync_dir(&src, &dest)?;

    let (tx, mut rx) = tokio::sync::mpsc::channel(1);

    let mut watcher = RecommendedWatcher::new(move |res: notify::Result<Event>| {
        if let Ok(event) = res {
            let _ = tx.blocking_send(event);
        }
    }, Config::default())?;

    watcher.watch(&src, RecursiveMode::Recursive)?;
    info!("Mirroring {:?} to {:?}", src, dest);

    while let Some(event) = rx.recv().await {
        debug!("Mirror event: {:?}", event);
        if let Err(e) = handle_mirror_event(event, &src, &dest) {
            error!("Mirror error: {}", e);
        }
    }

    Ok(())
}

fn sync_dir(src: &Path, dest: &Path) -> Result<()> {
    if !dest.exists() {
        fs::create_dir_all(dest)?;
    }
    
    let mut options = fs_extra::dir::CopyOptions::new();
    options.overwrite = true;
    options.copy_inside = true;
    
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            fs_extra::dir::copy(&path, dest, &options)?;
        } else {
            fs::copy(&path, dest.join(path.file_name().unwrap()))?;
        }
    }
    Ok(())
}

fn handle_mirror_event(event: Event, src_root: &Path, dest_root: &Path) -> Result<()> {
    for path in event.paths {
        let rel_path = path.strip_prefix(src_root).map_err(|e| anyhow!(e))?;
        let target_path = dest_root.join(rel_path);

        match event.kind {
            EventKind::Create(_) | EventKind::Modify(_) => {
                if path.is_dir() {
                    fs::create_dir_all(&target_path)?;
                } else if path.is_file() {
                    if let Some(parent) = target_path.parent() {
                        fs::create_dir_all(parent)?;
                    }
                    fs::copy(&path, &target_path)?;
                }
            }
            EventKind::Remove(_) => {
                if target_path.exists() {
                    if target_path.is_dir() {
                        fs::remove_dir_all(&target_path)?;
                    } else {
                        fs::remove_file(&target_path)?;
                    }
                }
            }
            _ => {}
        }
    }
    Ok(())
}
