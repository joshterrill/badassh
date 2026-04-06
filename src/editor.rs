use anyhow::{Context, Result};
use log::{debug, error, info};
use notify::{Config, Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use crate::transfer::SftpSessionInfo;

/// Tracks a remote file that's being edited locally
#[derive(Debug, Clone)]
pub struct RemoteFileMapping {
    #[allow(dead_code)]
    pub local_tmp_path: PathBuf,
    pub remote_path: String,
    pub session_info: SftpSessionInfo,
    pub last_modified: SystemTime,
}

/// Pending upload info for the transfer manager
#[derive(Debug, Clone)]
pub struct PendingEditorUpload {
    pub local_path: PathBuf,
    pub remote_path: String,
    pub session_info: SftpSessionInfo,
}

/// Manages file editing and syncing
pub struct EditorManager {
    /// Map of local tmp paths to remote file info
    remote_files: Arc<Mutex<HashMap<PathBuf, RemoteFileMapping>>>,
    /// Queue of pending uploads to be processed by TransferManager
    pending_uploads: Arc<Mutex<Vec<PendingEditorUpload>>>,
    /// Tmp directory for remote files
    tmp_dir: PathBuf,
    /// File watcher for remote file changes (tmp directory)
    #[allow(dead_code)]
    remote_watcher: Option<RecommendedWatcher>,
    /// File watcher for local directory changes
    local_watcher: Option<RecommendedWatcher>,
    /// Currently watched local directory
    watched_local_dir: Arc<Mutex<Option<PathBuf>>>,
    /// Flag indicating local directory changed
    local_changed: Arc<AtomicBool>,
}

/// Detect the default editor command
pub fn detect_default_editor() -> String {
    // Check for common editors in order of preference
    let editors = ["code", "cursor", "vim", "nano", "vi"];

    for editor in &editors {
        if std::process::Command::new("which")
            .arg(editor)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
        {
            return editor.to_string();
        }
    }

    // Fallback to "open" on macOS
    #[cfg(target_os = "macos")]
    return "open".to_string();

    #[cfg(not(target_os = "macos"))]
    return "vim".to_string();
}

impl EditorManager {
    pub fn new() -> Result<Self> {
        // Create tmp directory for remote files
        let tmp_dir = std::env::temp_dir().join("badassh-editor");
        fs::create_dir_all(&tmp_dir)?;

        let remote_files: Arc<Mutex<HashMap<PathBuf, RemoteFileMapping>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let pending_uploads: Arc<Mutex<Vec<PendingEditorUpload>>> =
            Arc::new(Mutex::new(Vec::new()));
        let local_changed = Arc::new(AtomicBool::new(false));
        let watched_local_dir = Arc::new(Mutex::new(None::<PathBuf>));

        // Set up file watcher for remote files (tmp directory)
        let watch_files = remote_files.clone();
        let watch_pending = pending_uploads.clone();
        let remote_watcher =
            Self::setup_remote_watcher(watch_files, watch_pending, tmp_dir.clone())?;

        Ok(Self {
            remote_files,
            pending_uploads,
            tmp_dir,
            remote_watcher: Some(remote_watcher),
            local_watcher: None,
            watched_local_dir,
            local_changed,
        })
    }

    fn setup_remote_watcher(
        remote_files: Arc<Mutex<HashMap<PathBuf, RemoteFileMapping>>>,
        pending_uploads: Arc<Mutex<Vec<PendingEditorUpload>>>,
        tmp_dir: PathBuf,
    ) -> Result<RecommendedWatcher> {
        let tmp_dir_for_closure = tmp_dir.clone();
        let mut watcher = RecommendedWatcher::new(
            move |res: Result<Event, notify::Error>| {
                match res {
                    Ok(event) => {
                        // Handle Modify, Create (for atomic saves), and Rename events
                        let should_check =
                            matches!(event.kind, EventKind::Modify(_) | EventKind::Create(_));

                        if should_check {
                            for path in &event.paths {
                                // Try to find the file in our tracked files
                                // Canonicalize the path to handle symlinks and relative paths
                                let canonical =
                                    path.canonicalize().unwrap_or_else(|_| path.clone());

                                debug!(
                                    "File event {:?} for path: {:?} (canonical: {:?})",
                                    event.kind, path, canonical
                                );

                                let mut files = remote_files.lock();

                                // Try both the original path and canonical path
                                let mapping_key = if files.contains_key(&canonical) {
                                    Some(canonical.clone())
                                } else if files.contains_key(path) {
                                    Some(path.clone())
                                } else {
                                    // Also try finding by filename within tmp_dir
                                    files
                                        .iter()
                                        .find(|(k, _)| {
                                            k.file_name() == path.file_name()
                                                && path.starts_with(&tmp_dir_for_closure)
                                        })
                                        .map(|(k, _)| k.clone())
                                };

                                if let Some(key) = mapping_key {
                                    if let Some(mapping) = files.get_mut(&key) {
                                        // Check if file was actually modified
                                        if let Ok(metadata) = fs::metadata(path) {
                                            if let Ok(modified) = metadata.modified() {
                                                if modified > mapping.last_modified {
                                                    info!("File modified, queueing upload: {:?} -> {}", path, mapping.remote_path);

                                                    // Update the last_modified time
                                                    mapping.last_modified = modified;

                                                    // Queue the upload
                                                    let upload = PendingEditorUpload {
                                                        local_path: path.clone(),
                                                        remote_path: mapping.remote_path.clone(),
                                                        session_info: mapping.session_info.clone(),
                                                    };
                                                    pending_uploads.lock().push(upload);
                                                }
                                            }
                                        }
                                    }
                                } else {
                                    debug!("No mapping found for path: {:?}", path);
                                }
                            }
                        }
                    }
                    Err(e) => {
                        error!("Watch error: {:?}", e);
                    }
                }
            },
            Config::default().with_poll_interval(Duration::from_millis(500)),
        )?;

        // Watch the tmp directory
        watcher.watch(&tmp_dir, RecursiveMode::Recursive)?;
        info!("Watching tmp directory: {:?}", tmp_dir);

        Ok(watcher)
    }

    /// Watch a local directory for changes
    pub fn watch_local_directory(&mut self, dir: &Path) -> Result<()> {
        let dir = dir.to_path_buf();

        // Check if already watching this directory
        {
            let watched = self.watched_local_dir.lock();
            if let Some(ref current) = *watched {
                if current == &dir {
                    return Ok(());
                }
            }
        }

        // Set up new watcher
        let local_changed = self.local_changed.clone();
        let watched_dir = dir.clone();

        let watcher = RecommendedWatcher::new(
            move |res: Result<Event, notify::Error>| {
                match res {
                    Ok(event) => {
                        // Check if the event is for a file in the watched directory (not subdirectory)
                        let is_relevant = event
                            .paths
                            .iter()
                            .any(|p| p.parent() == Some(watched_dir.as_path()));

                        if is_relevant
                            && matches!(
                                event.kind,
                                EventKind::Modify(_) | EventKind::Create(_) | EventKind::Remove(_)
                            )
                        {
                            debug!("Local directory changed: {:?}", event);
                            local_changed.store(true, Ordering::SeqCst);
                        }
                    }
                    Err(e) => {
                        error!("Local watch error: {:?}", e);
                    }
                }
            },
            Config::default().with_poll_interval(Duration::from_millis(500)),
        )?;

        let mut watcher = watcher;
        watcher.watch(&dir, RecursiveMode::NonRecursive)?;
        info!("Watching local directory: {:?}", dir);

        self.local_watcher = Some(watcher);
        *self.watched_local_dir.lock() = Some(dir);

        Ok(())
    }

    /// Check if local directory has changed and reset the flag
    pub fn check_local_changed(&self) -> bool {
        self.local_changed.swap(false, Ordering::SeqCst)
    }

    /// Take all pending uploads (clears the queue and returns the uploads)
    pub fn take_pending_uploads(&self) -> Vec<PendingEditorUpload> {
        std::mem::take(&mut *self.pending_uploads.lock())
    }

    /// Open a local file in the configured editor
    pub fn open_local_file(&self, path: &Path, editor_command: &str) -> Result<()> {
        info!(
            "Opening local file: {:?} with editor: {}",
            path, editor_command
        );

        if !path.exists() {
            anyhow::bail!("File does not exist: {:?}", path);
        }

        let trimmed = editor_command.trim();

        let result = if trimmed.is_empty() || trimmed == "open" {
            #[cfg(target_os = "macos")]
            {
                std::process::Command::new("open").arg(path).spawn()
            }

            #[cfg(target_os = "linux")]
            {
                std::process::Command::new("xdg-open").arg(path).spawn()
            }

            #[cfg(target_os = "windows")]
            {
                std::process::Command::new("cmd")
                    .args(["/C", "start", "", &path.to_string_lossy()])
                    .spawn()
            }
        } else {
            std::process::Command::new(trimmed).arg(path).spawn()
        };

        match result {
            Ok(child) => {
                info!(
                    "Successfully spawned editor process (pid: {:?})",
                    child.id()
                );
                Ok(())
            }
            Err(e) => {
                error!(
                    "Failed to spawn editor: {} - Error: {}",
                    if trimmed.is_empty() {
                        "system default"
                    } else {
                        trimmed
                    },
                    e
                );
                Err(anyhow::anyhow!(
                    "Failed to open editor '{}': {}",
                    if trimmed.is_empty() {
                        "system default"
                    } else {
                        trimmed
                    },
                    e
                ))
            }
        }
    }

    /// Download a remote file to tmp and open it in the configured editor
    pub fn open_remote_file(
        &self,
        session_info: &SftpSessionInfo,
        remote_path: &str,
        sftp: &ssh2::Sftp,
        editor_command: &str,
    ) -> Result<()> {
        info!("Opening remote file: {}", remote_path);

        // Create unique tmp path preserving filename
        let filename = Path::new(remote_path)
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "file".to_string());

        // Use a subdirectory based on host to avoid conflicts
        let host_dir = self.tmp_dir.join(&session_info.host);
        fs::create_dir_all(&host_dir)?;

        // Preserve directory structure in tmp
        let remote_dir = Path::new(remote_path).parent().unwrap_or(Path::new("/"));
        let tmp_subdir = host_dir.join(remote_dir.to_string_lossy().trim_start_matches('/'));
        fs::create_dir_all(&tmp_subdir)?;

        let tmp_path = tmp_subdir.join(&filename);

        // Download file
        debug!("Downloading to {:?}", tmp_path);
        let mut remote_file = sftp
            .open(Path::new(remote_path))
            .with_context(|| format!("Failed to open remote file: {}", remote_path))?;

        let mut content = Vec::new();
        remote_file.read_to_end(&mut content)?;

        fs::write(&tmp_path, &content)?;

        // Canonicalize AFTER writing the file so it exists
        let canonical_tmp_path = tmp_path.canonicalize().unwrap_or_else(|_| tmp_path.clone());

        // Get the actual modification time of the file we just wrote
        // Add a small buffer (2 seconds) to ignore any immediate events from the editor opening the file
        let last_modified = fs::metadata(&tmp_path)?
            .modified()?
            .checked_add(Duration::from_secs(2))
            .unwrap_or_else(SystemTime::now);

        // Track this file using canonical path
        let mapping = RemoteFileMapping {
            local_tmp_path: canonical_tmp_path.clone(),
            remote_path: remote_path.to_string(),
            session_info: session_info.clone(),
            last_modified,
        };

        {
            let mut files = self.remote_files.lock();
            info!(
                "Tracking remote file: {:?} -> {}",
                canonical_tmp_path, remote_path
            );
            files.insert(canonical_tmp_path.clone(), mapping);
        }

        // Open in editor
        self.open_local_file(&tmp_path, editor_command)?;

        Ok(())
    }

    /// Get the number of tracked remote files
    #[allow(dead_code)]
    pub fn tracked_file_count(&self) -> usize {
        self.remote_files.lock().len()
    }
}
