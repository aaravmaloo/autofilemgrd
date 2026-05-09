use std::path::{Path, PathBuf};
use std::process::Command;
use anyhow::{Result, anyhow};
use tracing::{info, error};

pub struct RemoteClient {
    user_host: String,
    port: u16,
}

impl RemoteClient {
    pub fn connect(remote: &str, port: u16) -> Result<Self> {
        // remote format: user@host
        if !remote.contains('@') {
            return Err(anyhow!("Invalid remote format. Use user@host"));
        }
        
        // Test connection
        let status = Command::new("ssh")
            .arg("-p")
            .arg(port.to_string())
            .arg("-o")
            .arg("BatchMode=yes")
            .arg("-o")
            .arg("ConnectTimeout=5")
            .arg(remote)
            .arg("exit")
            .status()?;

        if !status.success() {
            info!("Warning: Could not verify SSH connection to {}. Ensure keys are set up.", remote);
        }

        Ok(RemoteClient { 
            user_host: remote.to_string(), 
            port 
        })
    }

    pub fn copy_file(&self, local_path: &Path, remote_path: &str) -> Result<()> {
        let status = Command::new("scp")
            .arg("-P")
            .arg(self.port.to_string())
            .arg(local_path)
            .arg(format!("{}:{}", self.user_host, remote_path))
            .status()?;
            
        if !status.success() {
            return Err(anyhow!("scp failed with status {}", status));
        }
        Ok(())
    }

    pub fn remove_file(&self, remote_path: &str) -> Result<()> {
        let status = Command::new("ssh")
            .arg("-p")
            .arg(self.port.to_string())
            .arg(&self.user_host)
            .arg(format!("rm -f \"{}\"", remote_path))
            .status()?;
            
        if !status.success() {
            return Err(anyhow!("ssh rm failed with status {}", status));
        }
        Ok(())
    }

    pub fn create_dir(&self, remote_path: &str) -> Result<()> {
        let status = Command::new("ssh")
            .arg("-p")
            .arg(self.port.to_string())
            .arg(&self.user_host)
            .arg(format!("mkdir -p \"{}\"", remote_path))
            .status()?;
            
        if !status.success() {
            return Err(anyhow!("ssh mkdir failed with status {}", status));
        }
        Ok(())
    }

    pub fn sync_dir(&self, local_src: &Path, remote_dest: &str) -> Result<()> {
        self.create_dir(remote_dest)?;
        for entry in std::fs::read_dir(local_src)? {
            let entry = entry?;
            let path = entry.path();
            let status = Command::new("scp")
                .arg("-r")
                .arg("-P")
                .arg(self.port.to_string())
                .arg(&path)
                .arg(format!("{}:{}", self.user_host, remote_dest))
                .status()?;
                
            if !status.success() {
                error!("Initial sync failed for {:?}", path);
            }
        }
        Ok(())
    }
}

pub async fn run_remote_mirror_watcher(src: PathBuf, mut remote_dest: String, remote: String, port: u16) -> Result<()> {
    // Normalize tilde to current directory (home) for SSH commands
    if remote_dest.starts_with("~/") {
        remote_dest = remote_dest.replacen("~/", "./", 1);
    } else if remote_dest == "~" {
        remote_dest = ".".to_string();
    }

    use notify::{Config, RecommendedWatcher, RecursiveMode, Watcher, Event, EventKind};
    
    let client = RemoteClient::connect(&remote, port)?;
    info!("Connected (verified) to remote: {}", remote);

    info!("Performing initial sync to remote...");
    if let Err(e) = client.sync_dir(&src, &remote_dest) {
        error!("Initial sync error: {}", e);
    }

    let (tx, mut rx) = tokio::sync::mpsc::channel(1);

    let mut watcher = RecommendedWatcher::new(move |res: notify::Result<Event>| {
        if let Ok(event) = res {
            let _ = tx.blocking_send(event);
        }
    }, Config::default())?;

    watcher.watch(&src, RecursiveMode::Recursive)?;
    info!("Mirroring {:?} to remote {}:{}", src, remote, remote_dest);

    while let Some(event) = rx.recv().await {
        for path in event.paths {
            let rel_path = path.strip_prefix(&src).map_err(|e| anyhow!(e))?;
            let remote_path = format!("{}/{}", remote_dest, rel_path.to_string_lossy().replace("\\", "/"));

            match event.kind {
                EventKind::Create(_) | EventKind::Modify(_) => {
                    if path.is_dir() {
                        if let Err(e) = client.create_dir(&remote_path) {
                            error!("Remote mkdir error: {}", e);
                        }
                    } else if path.is_file() {
                        if let Some(idx) = remote_path.rfind('/') {
                            let parent_dir = &remote_path[..idx];
                            if let Err(e) = client.create_dir(parent_dir) {
                                error!("Failed to create parent dir {}: {}", parent_dir, e);
                            }
                        }
                        if let Err(e) = client.copy_file(&path, &remote_path) {
                            error!("Remote copy error: {}", e);
                        }
                    }
                }
                EventKind::Remove(_) => {
                    if let Err(e) = client.remove_file(&remote_path) {
                        error!("Remote rm error: {}", e);
                    }
                }
                _ => {}
            }
        }
    }

    Ok(())
}
