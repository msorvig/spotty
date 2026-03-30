//! Local state tracking for spotty instances.
//!
//! Tracks which instances are running and in which regions,
//! so the watcher knows where to look when cleaning up.

use anyhow::{Context, Result};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::PathBuf;

/// State file location: ~/.cache/spotty/instances.json
fn state_path() -> Result<PathBuf> {
    let cache_dir = dirs::cache_dir()
        .ok_or_else(|| anyhow::anyhow!("Cannot determine cache directory"))?
        .join("spotty");
    fs::create_dir_all(&cache_dir)?;
    Ok(cache_dir.join("instances.json"))
}

/// Open state file with exclusive lock for read-modify-write operations.
fn open_locked() -> Result<File> {
    let path = state_path()?;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .with_context(|| format!("Failed to open {}", path.display()))?;

    // Acquire exclusive lock (blocks until available)
    file.lock_exclusive()
        .with_context(|| format!("Failed to lock {}", path.display()))?;

    Ok(file)
}

/// Tracked instance info.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrackedInstance {
    pub region: String,
    pub auto_cleanup: bool,
}

/// State file format.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct State {
    /// Map of instance_id -> TrackedInstance
    pub instances: HashMap<String, TrackedInstance>,
}

impl State {
    /// Load state from disk, or return empty state if file doesn't exist.
    /// Uses a shared lock for reading.
    pub fn load() -> Result<Self> {
        let path = state_path()?;
        if !path.exists() {
            return Ok(Self::default());
        }

        let file = File::open(&path)
            .with_context(|| format!("Failed to open {}", path.display()))?;

        // Acquire shared lock for reading
        file.lock_shared()
            .with_context(|| format!("Failed to lock {}", path.display()))?;

        let content = fs::read_to_string(&path)
            .with_context(|| format!("Failed to read {}", path.display()))?;

        // Lock released when file is dropped
        if content.is_empty() {
            return Ok(Self::default());
        }

        let state: State = serde_json::from_str(&content)
            .with_context(|| format!("Failed to parse {}", path.display()))?;
        Ok(state)
    }

    /// Load state with exclusive lock, for read-modify-write operations.
    /// Returns the state and the locked file handle.
    fn load_locked() -> Result<(Self, File)> {
        let mut file = open_locked()?;

        let mut content = String::new();
        file.read_to_string(&mut content)?;

        let state = if content.is_empty() {
            Self::default()
        } else {
            serde_json::from_str(&content)
                .with_context(|| "Failed to parse state file")?
        };

        Ok((state, file))
    }

    /// Save state to an already-locked file.
    fn save_to_locked(&self, mut file: &File) -> Result<()> {
        use std::io::Seek;

        // Truncate and seek to beginning
        file.set_len(0)?;
        file.seek(std::io::SeekFrom::Start(0))?;

        let content = serde_json::to_string_pretty(self)?;
        file.write_all(content.as_bytes())?;
        file.flush()?;

        // Lock released when file is dropped
        Ok(())
    }

    /// Save state to disk (acquires exclusive lock).
    pub fn save(&self) -> Result<()> {
        let file = open_locked()?;
        self.save_to_locked(&file)
    }

    /// Add a tracked instance.
    pub fn add_instance(&mut self, instance_id: &str, region: &str, auto_cleanup: bool) {
        self.instances.insert(
            instance_id.to_string(),
            TrackedInstance {
                region: region.to_string(),
                auto_cleanup,
            },
        );
    }

    /// Remove a tracked instance.
    pub fn remove_instance(&mut self, instance_id: &str) {
        self.instances.remove(instance_id);
    }

    /// Get all regions that have tracked instances.
    pub fn get_all_regions(&self) -> Vec<String> {
        self.instances
            .values()
            .map(|i| i.region.clone())
            .collect::<HashSet<_>>()
            .into_iter()
            .collect()
    }

    /// Get all regions that have auto-cleanup instances.
    pub fn get_auto_cleanup_regions(&self) -> Vec<String> {
        self.instances
            .values()
            .filter(|i| i.auto_cleanup)
            .map(|i| i.region.clone())
            .collect::<HashSet<_>>()
            .into_iter()
            .collect()
    }

    /// Get all instance IDs with auto-cleanup enabled, grouped by region.
    pub fn get_auto_cleanup_instances_by_region(&self) -> HashMap<String, Vec<String>> {
        let mut by_region: HashMap<String, Vec<String>> = HashMap::new();
        for (id, info) in &self.instances {
            if info.auto_cleanup {
                by_region
                    .entry(info.region.clone())
                    .or_default()
                    .push(id.clone());
            }
        }
        by_region
    }
}

/// Track a newly started instance (atomic with file locking).
pub fn track_instance(instance_id: &str, region: &str, auto_cleanup: bool) -> Result<()> {
    let (mut state, file) = State::load_locked()?;
    state.add_instance(instance_id, region, auto_cleanup);
    state.save_to_locked(&file)?;
    Ok(())
}

/// Remove a terminated instance from tracking (atomic with file locking).
pub fn untrack_instance(instance_id: &str) -> Result<()> {
    let (mut state, file) = State::load_locked()?;
    state.remove_instance(instance_id);
    state.save_to_locked(&file)?;
    Ok(())
}

/// Remove multiple terminated instances from tracking (atomic with file locking).
pub fn untrack_instances(instance_ids: &[String]) -> Result<()> {
    let (mut state, file) = State::load_locked()?;
    for id in instance_ids {
        state.remove_instance(id);
    }
    state.save_to_locked(&file)?;
    Ok(())
}
