use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;
use anyhow::Result;
use sysinfo::{Pid, System};

#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum TaskType {
    Unzip { path: PathBuf, delete: bool },
    Mirror { src: PathBuf, dest: PathBuf, remote: Option<String>, port: Option<u16>, #[serde(default)] use_rsync: bool },
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct DaemonTask {
    pub pid: u32,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
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

    fn state_path(&self) -> PathBuf {
        self.config_dir.join("state.json")
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
        let tasks: Vec<DaemonTask> = serde_json::from_str(&content)?;
        Ok(tasks)
    }

    pub fn get_active_tasks(&self) -> Result<Vec<DaemonTask>> {
        let tasks = self.load_tasks()?;
        let mut system = System::new_all();
        system.refresh_processes();
        
        let active_tasks: Vec<DaemonTask> = tasks
            .into_iter()
            .filter(|t| system.process(Pid::from(t.pid as usize)).is_some())
            .collect();
            
        Ok(active_tasks)
    }

    pub fn is_process_running(&self, pid: u32) -> bool {
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
        tasks.retain(|t| t.pid != task.pid);
        tasks.push(task);
        self.save_tasks(&tasks)?;
        Ok(())
    }

    pub fn set_task_enabled(&self, pid: u32, enabled: bool) -> Result<bool> {
        let mut tasks = self.load_tasks()?;
        let mut changed = false;

        for task in &mut tasks {
            if task.pid == pid {
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

    pub fn remove_task(&self, pid: u32) -> Result<()> {
        let mut tasks = self.load_tasks()?;
        tasks.retain(|t| t.pid != pid);
        self.save_tasks(&tasks)?;
        Ok(())
    }
}
