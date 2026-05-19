use serde::{Deserialize, Serialize};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use anyhow::Result;

#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum TaskType {
    Unzip {
        path: PathBuf,
        delete: bool,
        #[serde(default = "default_conflict_policy")]
        conflict: ConflictPolicy,
        #[serde(default = "default_stable_secs")]
        stable_secs: u64,
    },
    Mirror {
        src: PathBuf,
        dest: PathBuf,
        remote: Option<String>,
        port: Option<u16>,
        #[serde(default)]
        use_rsync: bool,
        #[serde(default = "default_conflict_policy")]
        conflict: ConflictPolicy,
        #[serde(default = "default_stable_secs")]
        stable_secs: u64,
        #[serde(default)]
        include: Vec<String>,
        #[serde(default)]
        exclude: Vec<String>,
    },
    Sort {
        path: PathBuf,
        rules: Vec<String>,
        #[serde(default = "default_conflict_policy")]
        conflict: ConflictPolicy,
        #[serde(default = "default_stable_secs")]
        stable_secs: u64,
    },
    Clean {
        path: PathBuf,
        older_than_days: u64,
        move_to: Option<PathBuf>,
        remote: Option<String>,
        port: Option<u16>,
        #[serde(default)]
        use_rsync: bool,
        #[serde(default)]
        empty_dirs: bool,
        #[serde(default = "default_conflict_policy")]
        conflict: ConflictPolicy,
    },
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum ConflictPolicy {
    Skip,
    Overwrite,
    Rename,
    Newest,
}

fn default_conflict_policy() -> ConflictPolicy {
    ConflictPolicy::Rename
}

fn default_stable_secs() -> u64 {
    2
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct DaemonTask {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
    pub pid: u32,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    #[serde(default)]
    pub paused_until: Option<i64>,
    pub task: TaskType,
}

fn default_enabled() -> bool {
    true
}

pub struct StateManager {
    config_dir: PathBuf,
}

impl StateManager {
    pub fn new() -> Result<Self> {
        let config_dir = dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("autofilemgrd");
        
        if !config_dir.exists() {
            fs::create_dir_all(&config_dir)?;
        }
        
        Ok(StateManager { config_dir })
    }

    pub fn config_dir(&self) -> PathBuf {
        self.config_dir.clone()
    }

    fn state_path(&self) -> PathBuf {
        self.config_dir.join("state.json")
    }

    pub fn history_path(&self) -> PathBuf {
        self.config_dir.join("history.jsonl")
    }

    pub fn undo_path(&self) -> PathBuf {
        self.config_dir.join("undo.jsonl")
    }

    pub fn log_path(&self, pid: u32) -> PathBuf {
        self.config_dir.join("logs").join(format!("{}.log", pid))
    }

    pub fn ensure_log_dir(&self) -> Result<()> {
        let dir = self.config_dir.join("logs");
        if !dir.exists() {
            fs::create_dir_all(&dir)?;
        }
        Ok(())
    }

    pub fn load_tasks(&self) -> Result<Vec<DaemonTask>> {
        let path = self.state_path();
        if !path.exists() {
            return Ok(Vec::new());
        }
        
        let content = fs::read_to_string(path)?;
        let mut tasks: Vec<DaemonTask> = serde_json::from_str(&content)?;
        let mut changed = false;
        for task in &mut tasks {
            if task.id.is_empty() {
                task.id = generate_task_id();
                changed = true;
            }
        }
        if changed {
            self.save_tasks(&tasks)?;
        }
        Ok(tasks)
    }

    pub fn is_process_running(&self, pid: u32) -> bool {
        use sysinfo::{Pid, System};
        let mut system = System::new();
        system.refresh_processes();
        system.process(Pid::from(pid as usize)).is_some()
    }

    pub fn save_tasks(&self, tasks: &[DaemonTask]) -> Result<()> {
        let content = serde_json::to_string_pretty(tasks)?;
        fs::write(self.state_path(), content)?;
        Ok(())
    }

    pub fn add_task(&self, task: DaemonTask) -> Result<()> {
        let mut tasks = self.load_tasks()?;
        // Remove any existing task with the same PID or same configuration to avoid duplicates
        tasks.retain(|t| t.pid != task.pid && t.id != task.id);
        tasks.push(task);
        self.save_tasks(&tasks)?;
        Ok(())
    }

    pub fn set_task_enabled(&self, target: &str, enabled: bool) -> Result<bool> {
        let mut tasks = self.load_tasks()?;
        let mut changed = false;

        for task in &mut tasks {
            if task_matches(task, target) {
                task.enabled = enabled;
                changed = true;
                break;
            }
        }

        if changed {
            self.save_tasks(&tasks)?;
        }

        Ok(changed)
    }

    pub fn set_task_paused_until(&self, target: &str, paused_until: Option<i64>) -> Result<bool> {
        let mut tasks = self.load_tasks()?;
        let mut changed = false;

        for task in &mut tasks {
            if task_matches(task, target) {
                task.paused_until = paused_until;
                changed = true;
                break;
            }
        }

        if changed {
            self.save_tasks(&tasks)?;
        }

        Ok(changed)
    }

    pub fn set_all_tasks_enabled(&self, enabled: bool) -> Result<usize> {
        let mut tasks = self.load_tasks()?;
        for task in &mut tasks {
            task.enabled = enabled;
        }
        let count = tasks.len();
        self.save_tasks(&tasks)?;
        Ok(count)
    }

    pub fn remove_all_tasks(&self) -> Result<usize> {
        let tasks = self.load_tasks()?;
        let count = tasks.len();
        self.save_tasks(&[])?;
        Ok(count)
    }

    pub fn remove_task_by_pid(&self, pid: u32) -> Result<()> {
        let mut tasks = self.load_tasks()?;
        tasks.retain(|t| t.pid != pid);
        self.save_tasks(&tasks)?;
        Ok(())
    }

    pub fn remove_task(&self, target: &str) -> Result<bool> {
        let mut tasks = self.load_tasks()?;
        let before = tasks.len();
        tasks.retain(|t| !task_matches(t, target));
        self.save_tasks(&tasks)?;
        Ok(tasks.len() != before)
    }

    pub fn find_task(&self, target: &str) -> Result<Option<DaemonTask>> {
        Ok(self.load_tasks()?.into_iter().find(|task| task_matches(task, target)))
    }

    pub fn append_history(&self, event: &HistoryEvent) -> Result<()> {
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.history_path())?;
        writeln!(file, "{}", serde_json::to_string(event)?)?;
        Ok(())
    }

    pub fn append_undo(&self, event: &UndoEvent) -> Result<()> {
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.undo_path())?;
        writeln!(file, "{}", serde_json::to_string(event)?)?;
        Ok(())
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct HistoryEvent {
    pub ts: i64,
    pub task: Option<String>,
    pub level: String,
    pub message: String,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "action")]
pub enum UndoEvent {
    Move { src: PathBuf, dest: PathBuf },
}

pub fn generate_task_id() -> String {
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or_default();
    format!("task_{}_{}", millis, std::process::id())
}

pub fn task_matches(task: &DaemonTask, target: &str) -> bool {
    task.id == target
        || task.name.as_deref() == Some(target)
        || task.pid.to_string() == target
}
