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

    pub fn remove_path(&self, remote_path: &str) -> Result<()> {
        let status = Command::new("ssh")
            .arg("-p")
            .arg(self.port.to_string())
            .arg(&self.user_host)
            .arg(format!("rm -rf \"{}\"", remote_path))
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
        if !src.exists() {
            info!("Source directory {:?} deleted. Stopping mirror.", src);
            break;
        }

        for path in event.paths {
            let rel_path = match path.strip_prefix(&src) {
                Ok(p) => p,
                Err(_) => continue, // Path is not within src (e.g. src itself was deleted and notify sent a path we can't strip)
            };
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
                    // To avoid spawning thousands of processes for a recursive delete, 
                    // we could check if the path's parent still exists locally.
                    // But for now, let's just use rm -rf which is safer.
                    if let Err(e) = client.remove_path(&remote_path) {
                        error!("Remote rm error: {}", e);
                    }
                }
                _ => {}
            }
        }
    }

    Ok(())
}

fn normalize_remote_dest(mut remote_dest: String) -> String {
    if remote_dest.starts_with("~/") {
        remote_dest = remote_dest.replacen("~/", "./", 1);
    } else if remote_dest == "~" {
        remote_dest = ".".to_string();
    }
    remote_dest
}

fn single_quote_for_shell(input: &str) -> String {
    input.replace('\'', "'\"'\"'")
}

fn command_error(cmd_label: &str, output: &std::process::Output) -> anyhow::Error {
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    anyhow!(
        "{} failed with status {:?}\nstdout:\n{}\nstderr:\n{}",
        cmd_label,
        output.status.code(),
        if stdout.is_empty() { "<empty>" } else { &stdout },
        if stderr.is_empty() { "<empty>" } else { &stderr },
    )
}

#[cfg(windows)]
fn windows_path_to_wsl(path: &Path) -> Result<String> {
    let mut raw = path.to_string_lossy().to_string();
    if let Some(stripped) = raw.strip_prefix(r"\\?\") {
        raw = stripped.to_string();
    }
    if let Some(stripped) = raw.strip_prefix("//?/") {
        raw = stripped.to_string();
    }
    let raw = raw.replace('\\', "/");
    if raw.len() >= 3 && raw.as_bytes()[1] == b':' && raw.as_bytes()[2] == b'/' {
        let drive = raw.chars().next().unwrap_or('c').to_ascii_lowercase();
        let rest = &raw[3..];
        Ok(format!("/mnt/{}/{}", drive, rest))
    } else {
        Err(anyhow!("Unsupported Windows path for WSL rsync: {}", raw))
    }
}

fn run_remote_rsync_once(src: &Path, remote_dest: &str, remote: &str, port: u16) -> Result<()> {
    #[cfg(windows)]
    {
        let src_unix = windows_path_to_wsl(src)?;
        let cmd = format!(
            "rsync -az --delete -e 'ssh -p {}' '{}/' '{}:{}/'",
            port,
            single_quote_for_shell(&src_unix),
            single_quote_for_shell(remote),
            single_quote_for_shell(remote_dest)
        );
        let output = Command::new("wsl")
            .args(["sh", "-lc", &cmd])
            .output()?;
        if !output.status.success() {
            return Err(command_error("wsl rsync", &output));
        }
    }
    #[cfg(not(windows))]
    {
        let src_unix = src.to_string_lossy().replace('\\', "/");
        let src_arg = format!("{}/", src_unix);
        let dest_arg = format!("{}:{}/", remote, remote_dest);
        let output = Command::new("rsync")
            .args(["-az", "--delete", "-e", &format!("ssh -p {}", port), &src_arg, &dest_arg])
            .output()?;
        if !output.status.success() {
            return Err(command_error("rsync", &output));
        }
    }
    Ok(())
}

pub async fn run_remote_rsync_watcher(src: PathBuf, remote_dest: String, remote: String, port: u16) -> Result<()> {
    let remote_dest = normalize_remote_dest(remote_dest);
    use notify::{Config, RecommendedWatcher, RecursiveMode, Watcher, Event};

    info!("Performing initial rsync to remote...");
    if let Err(e) = run_remote_rsync_once(&src, &remote_dest, &remote, port) {
        error!("Initial remote rsync failed: {}", e);
        return Err(e);
    }

    let (tx, mut rx) = tokio::sync::mpsc::channel(1);
    let mut watcher = RecommendedWatcher::new(move |res: notify::Result<Event>| {
        if let Ok(event) = res {
            let _ = tx.blocking_send(event);
        }
    }, Config::default())?;

    watcher.watch(&src, RecursiveMode::Recursive)?;
    info!("Mirroring {:?} to remote {}:{} using rsync", src, remote, remote_dest);

    while let Some(_event) = rx.recv().await {
        if !src.exists() {
            info!("Source directory {:?} deleted. Stopping remote rsync mirror.", src);
            break;
        }
        if let Err(e) = run_remote_rsync_once(&src, &remote_dest, &remote, port) {
            error!("Remote rsync error: {}", e);
        }
    }

    Ok(())
}
