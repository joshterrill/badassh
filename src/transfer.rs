use anyhow::{Context, Result};
use log::{error, info, warn};
use parking_lot::Mutex;
use ssh2::{Session, Sftp};
use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;

const CHUNK_SIZE: usize = 64 * 1024; // 64KB chunks
const MAX_RETRIES: u32 = 5;
const RETRY_DELAY_MS: u64 = 1000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransferStatus {
    Pending,
    InProgress {
        bytes_transferred: u64,
        total_bytes: u64,
    },
    Completed,
    Failed(String),
    Retrying {
        attempt: u32,
        reason: String,
    },
}

#[derive(Debug, Clone)]
pub struct TransferItem {
    pub id: usize,
    pub source_path: String,
    pub dest_path: String,
    pub is_download: bool,
    #[allow(dead_code)]
    pub is_dir: bool,
    pub status: TransferStatus,
    #[allow(dead_code)]
    pub total_bytes: u64,
    #[allow(dead_code)]
    pub bytes_transferred: u64,
}

pub struct TransferManager {
    items: Arc<Mutex<Vec<TransferItem>>>,
    next_id: Arc<Mutex<usize>>,
    active_transfers: Arc<Mutex<usize>>,
    transfer_progress: Arc<Mutex<TransferProgressState>>,
    max_parallel: usize,
}

#[derive(Debug, Clone, Default)]
struct TransferProgressState {
    total_count: usize,
    completed_count: usize,
    active_count: usize,
    total_bytes: u64,
    bytes_transferred: u64,
    download_count: usize,
    upload_count: usize,
}

#[derive(Debug, Clone)]
pub struct TransferProgressSnapshot {
    pub total_count: usize,
    pub completed_count: usize,
    pub total_bytes: u64,
    pub bytes_transferred: u64,
    pub download_count: usize,
    pub upload_count: usize,
}

impl TransferManager {
    pub fn new(max_parallel: usize) -> Self {
        Self {
            items: Arc::new(Mutex::new(Vec::new())),
            next_id: Arc::new(Mutex::new(1)),
            active_transfers: Arc::new(Mutex::new(0)),
            transfer_progress: Arc::new(Mutex::new(TransferProgressState::default())),
            max_parallel,
        }
    }

    pub fn get_items(&self) -> Vec<TransferItem> {
        self.items.lock().clone()
    }

    #[allow(dead_code)]
    pub fn has_active_transfers(&self) -> bool {
        *self.active_transfers.lock() > 0
    }

    #[allow(dead_code)]
    pub fn queue_download(
        &self,
        sftp_session: SftpSessionInfo,
        remote_path: String,
        local_dir: String,
        is_dir: bool,
    ) -> usize {
        let filename = Path::new(&remote_path)
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "download".to_string());

        let dest_path = format!("{}/{}", local_dir.trim_end_matches('/'), filename);

        self.queue_download_to_path(sftp_session, remote_path, dest_path, is_dir, None)
    }

    pub fn queue_download_to_path(
        &self,
        sftp_session: SftpSessionInfo,
        remote_path: String,
        dest_path: String,
        is_dir: bool,
        total_bytes: Option<u64>,
    ) -> usize {
        let id = {
            let mut next_id = self.next_id.lock();
            let id = *next_id;
            *next_id += 1;
            id
        };

        info!("Queuing download #{}: {} -> {}", id, remote_path, dest_path);

        let item = TransferItem {
            id,
            source_path: remote_path.clone(),
            dest_path: dest_path.clone(),
            is_download: true,
            is_dir,
            status: TransferStatus::Pending,
            total_bytes: total_bytes.unwrap_or(0),
            bytes_transferred: 0,
        };

        self.items.lock().push(item);
        if !is_dir {
            let mut progress = self.transfer_progress.lock();
            progress.total_count += 1;
            progress.active_count += 1;
            progress.total_bytes += total_bytes.unwrap_or(0);
            progress.download_count += 1;
        }

        // Start the transfer in a background thread
        let items = Arc::clone(&self.items);
        let active = Arc::clone(&self.active_transfers);
        let transfer_progress = Arc::clone(&self.transfer_progress);
        let max_parallel = self.max_parallel;

        thread::spawn(move || {
            // Wait until we can start
            loop {
                {
                    let count = *active.lock();
                    if count < max_parallel {
                        *active.lock() += 1;
                        break;
                    }
                }
                thread::sleep(std::time::Duration::from_millis(100));
            }

            let result = Self::execute_download(
                &items,
                &transfer_progress,
                id,
                &sftp_session,
                &remote_path,
                &dest_path,
                is_dir,
            );

            if let Err(e) = result {
                Self::mark_transfer_failed(&items, &transfer_progress, id, e.to_string());
            }

            *active.lock() -= 1;
        });

        id
    }

    #[allow(dead_code)]
    pub fn queue_upload(
        &self,
        sftp_session: SftpSessionInfo,
        local_path: String,
        remote_dir: String,
        is_dir: bool,
    ) -> usize {
        let filename = Path::new(&local_path)
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "upload".to_string());

        let dest_path = format!("{}/{}", remote_dir.trim_end_matches('/'), filename);

        let total_bytes = if is_dir {
            None
        } else {
            fs::metadata(&local_path)
                .ok()
                .map(|metadata| metadata.len())
        };

        self.queue_upload_to_path(sftp_session, local_path, dest_path, is_dir, total_bytes)
    }

    /// Queue an upload to a specific remote path (used for syncing edited files)
    pub fn queue_upload_to_path(
        &self,
        sftp_session: SftpSessionInfo,
        local_path: String,
        dest_path: String,
        is_dir: bool,
        total_bytes: Option<u64>,
    ) -> usize {
        let id = {
            let mut next_id = self.next_id.lock();
            let id = *next_id;
            *next_id += 1;
            id
        };

        info!("Queuing upload #{}: {} -> {}", id, local_path, dest_path);

        let item = TransferItem {
            id,
            source_path: local_path.clone(),
            dest_path: dest_path.clone(),
            is_download: false,
            is_dir,
            status: TransferStatus::Pending,
            total_bytes: total_bytes.unwrap_or(0),
            bytes_transferred: 0,
        };

        self.items.lock().push(item);
        if !is_dir {
            let mut progress = self.transfer_progress.lock();
            progress.total_count += 1;
            progress.active_count += 1;
            progress.total_bytes += total_bytes.unwrap_or(0);
            progress.upload_count += 1;
        }

        let items = Arc::clone(&self.items);
        let active = Arc::clone(&self.active_transfers);
        let transfer_progress = Arc::clone(&self.transfer_progress);
        let max_parallel = self.max_parallel;

        thread::spawn(move || {
            loop {
                {
                    let count = *active.lock();
                    if count < max_parallel {
                        *active.lock() += 1;
                        break;
                    }
                }
                thread::sleep(std::time::Duration::from_millis(100));
            }

            let result = Self::execute_upload(
                &items,
                &transfer_progress,
                id,
                &sftp_session,
                &local_path,
                &dest_path,
                is_dir,
            );

            if let Err(e) = result {
                Self::mark_transfer_failed(&items, &transfer_progress, id, e.to_string());
            }

            *active.lock() -= 1;
        });

        id
    }

    fn execute_download(
        items: &Arc<Mutex<Vec<TransferItem>>>,
        transfer_progress: &Arc<Mutex<TransferProgressState>>,
        id: usize,
        session_info: &SftpSessionInfo,
        remote_path: &str,
        local_path: &str,
        is_dir: bool,
    ) -> Result<()> {
        let mut attempt = 0;

        info!(
            "Starting download #{}: {} -> {}",
            id, remote_path, local_path
        );

        loop {
            attempt += 1;

            if attempt > 1 {
                warn!("Download #{} retry attempt {}", id, attempt);
            }

            // Update status
            {
                let mut items_guard = items.lock();
                if let Some(item) = items_guard.iter_mut().find(|i| i.id == id) {
                    if attempt > 1 {
                        item.status = TransferStatus::Retrying {
                            attempt,
                            reason: "Reconnecting...".to_string(),
                        };
                    } else {
                        item.status = TransferStatus::InProgress {
                            bytes_transferred: 0,
                            total_bytes: 0,
                        };
                    }
                }
            }

            // Create new session for this transfer
            let session = match Self::create_session(session_info) {
                Ok(s) => s,
                Err(e) => {
                    if attempt >= MAX_RETRIES {
                        return Err(e);
                    }
                    thread::sleep(std::time::Duration::from_millis(
                        RETRY_DELAY_MS * attempt as u64,
                    ));
                    continue;
                }
            };

            let sftp = session.sftp().context("Failed to create SFTP session")?;

            let result = if is_dir {
                Self::download_directory(
                    items,
                    transfer_progress,
                    id,
                    &sftp,
                    remote_path,
                    local_path,
                )
            } else {
                Self::download_file(items, transfer_progress, id, &sftp, remote_path, local_path)
            };

            match result {
                Ok(()) => {
                    info!("Download #{} completed successfully: {}", id, local_path);
                    Self::mark_transfer_completed(items, transfer_progress, id);
                    return Ok(());
                }
                Err(e) => {
                    if attempt >= MAX_RETRIES {
                        error!("Download #{} failed after {} attempts: {}", id, attempt, e);
                        return Err(e);
                    }
                    warn!("Download #{} attempt {} failed: {}", id, attempt, e);
                    {
                        let mut items_guard = items.lock();
                        if let Some(item) = items_guard.iter_mut().find(|i| i.id == id) {
                            item.status = TransferStatus::Retrying {
                                attempt,
                                reason: e.to_string(),
                            };
                        }
                    }
                    thread::sleep(std::time::Duration::from_millis(
                        RETRY_DELAY_MS * attempt as u64,
                    ));
                }
            }
        }
    }

    fn download_file(
        items: &Arc<Mutex<Vec<TransferItem>>>,
        transfer_progress: &Arc<Mutex<TransferProgressState>>,
        id: usize,
        sftp: &Sftp,
        remote_path: &str,
        local_path: &str,
    ) -> Result<()> {
        let remote_path = Path::new(remote_path);
        let local_path = Path::new(local_path);

        // Create temp file path
        let temp_path = local_path.with_file_name(format!(
            "$.{}.temp",
            local_path.file_name().unwrap_or_default().to_string_lossy()
        ));

        // Get file size
        let stat = sftp
            .stat(remote_path)
            .context("Failed to stat remote file")?;
        let total_bytes = stat.size.unwrap_or(0);

        Self::update_transfer_progress(
            items,
            transfer_progress,
            id,
            total_bytes,
            0,
            TransferStatus::InProgress {
                bytes_transferred: 0,
                total_bytes,
            },
        );

        // Check if temp file exists with partial data
        let resume_position = if temp_path.exists() {
            fs::metadata(&temp_path).map(|m| m.len()).unwrap_or(0)
        } else {
            0
        };

        // Create parent directory
        if let Some(parent) = local_path.parent() {
            fs::create_dir_all(parent)?;
        }

        // Open remote file
        let mut remote_file = sftp
            .open(remote_path)
            .context("Failed to open remote file")?;

        // Open/create local temp file
        let mut local_file = if resume_position > 0 {
            let f = fs::OpenOptions::new()
                .write(true)
                .append(true)
                .open(&temp_path)?;
            remote_file.seek(SeekFrom::Start(resume_position))?;
            f
        } else {
            File::create(&temp_path)?
        };

        let mut bytes_transferred = resume_position;
        Self::update_transfer_progress(
            items,
            transfer_progress,
            id,
            total_bytes,
            bytes_transferred,
            TransferStatus::InProgress {
                bytes_transferred,
                total_bytes,
            },
        );
        let mut buffer = vec![0u8; CHUNK_SIZE];

        loop {
            let bytes_read = remote_file.read(&mut buffer)?;
            if bytes_read == 0 {
                break;
            }

            local_file.write_all(&buffer[..bytes_read])?;
            bytes_transferred += bytes_read as u64;

            Self::update_transfer_progress(
                items,
                transfer_progress,
                id,
                total_bytes,
                bytes_transferred,
                TransferStatus::InProgress {
                    bytes_transferred,
                    total_bytes,
                },
            );
        }

        // Sync and close
        local_file.sync_all()?;
        drop(local_file);

        // Rename temp file to final name
        fs::rename(&temp_path, local_path).context("Failed to rename temp file")?;

        Ok(())
    }

    fn download_directory(
        items: &Arc<Mutex<Vec<TransferItem>>>,
        transfer_progress: &Arc<Mutex<TransferProgressState>>,
        id: usize,
        sftp: &Sftp,
        remote_path: &str,
        local_path: &str,
    ) -> Result<()> {
        let remote_path = Path::new(remote_path);
        let local_path = Path::new(local_path);

        // Create local directory
        fs::create_dir_all(local_path)?;

        // List remote directory
        let entries = sftp.readdir(remote_path)?;

        for (path, stat) in entries {
            let filename = path.file_name().unwrap_or_default().to_string_lossy();
            if filename == "." || filename == ".." {
                continue;
            }

            let remote_entry = path.to_string_lossy().to_string();
            let local_entry = local_path.join(&*filename).to_string_lossy().to_string();

            if stat.is_dir() {
                Self::download_directory(
                    items,
                    transfer_progress,
                    id,
                    sftp,
                    &remote_entry,
                    &local_entry,
                )?;
            } else {
                Self::download_file(
                    items,
                    transfer_progress,
                    id,
                    sftp,
                    &remote_entry,
                    &local_entry,
                )?;
            }
        }

        Ok(())
    }

    fn update_transfer_progress(
        items: &Arc<Mutex<Vec<TransferItem>>>,
        transfer_progress: &Arc<Mutex<TransferProgressState>>,
        id: usize,
        total_bytes: u64,
        bytes_transferred: u64,
        status: TransferStatus,
    ) {
        let (total_delta, bytes_delta) = {
            let mut items_guard = items.lock();
            if let Some(item) = items_guard.iter_mut().find(|i| i.id == id) {
                let previous_total = item.total_bytes;
                let previous_bytes = item.bytes_transferred;
                item.total_bytes = total_bytes;
                item.bytes_transferred = bytes_transferred;
                item.status = status;
                (
                    total_bytes as i128 - previous_total as i128,
                    bytes_transferred as i128 - previous_bytes as i128,
                )
            } else {
                (0, 0)
            }
        };

        if total_delta != 0 || bytes_delta != 0 {
            let mut progress = transfer_progress.lock();
            if total_delta >= 0 {
                progress.total_bytes += total_delta as u64;
            } else {
                progress.total_bytes = progress.total_bytes.saturating_sub((-total_delta) as u64);
            }

            if bytes_delta >= 0 {
                progress.bytes_transferred += bytes_delta as u64;
            } else {
                progress.bytes_transferred = progress
                    .bytes_transferred
                    .saturating_sub((-bytes_delta) as u64);
            }
        }
    }

    fn mark_transfer_completed(
        items: &Arc<Mutex<Vec<TransferItem>>>,
        transfer_progress: &Arc<Mutex<TransferProgressState>>,
        id: usize,
    ) {
        let final_delta = {
            let mut items_guard = items.lock();
            if let Some(item) = items_guard.iter_mut().find(|i| i.id == id) {
                let delta = item.total_bytes.saturating_sub(item.bytes_transferred);
                item.bytes_transferred = item.total_bytes;
                item.status = TransferStatus::Completed;
                Some(delta)
            } else {
                None
            }
        };

        if let Some(final_delta) = final_delta {
            let mut progress = transfer_progress.lock();
            progress.bytes_transferred += final_delta;
            progress.completed_count += 1;
            progress.active_count = progress.active_count.saturating_sub(1);
        }
    }

    fn mark_transfer_failed(
        items: &Arc<Mutex<Vec<TransferItem>>>,
        transfer_progress: &Arc<Mutex<TransferProgressState>>,
        id: usize,
        error: String,
    ) {
        let removed = {
            let mut items_guard = items.lock();
            if let Some(item) = items_guard.iter_mut().find(|i| i.id == id) {
                item.status = TransferStatus::Failed(error);
                Some((item.total_bytes, item.bytes_transferred, item.is_download))
            } else {
                None
            }
        };

        if let Some((total_bytes, bytes_transferred, is_download)) = removed {
            let mut progress = transfer_progress.lock();
            progress.active_count = progress.active_count.saturating_sub(1);
            progress.total_count = progress.total_count.saturating_sub(1);
            progress.total_bytes = progress.total_bytes.saturating_sub(total_bytes);
            progress.bytes_transferred =
                progress.bytes_transferred.saturating_sub(bytes_transferred);
            if is_download {
                progress.download_count = progress.download_count.saturating_sub(1);
            } else {
                progress.upload_count = progress.upload_count.saturating_sub(1);
            }
        }
    }

    pub fn transfer_progress(&self) -> Option<TransferProgressSnapshot> {
        let progress = self.transfer_progress.lock();
        if progress.total_count == 0 {
            return None;
        }

        Some(TransferProgressSnapshot {
            total_count: progress.total_count,
            completed_count: progress.completed_count,
            total_bytes: progress.total_bytes,
            bytes_transferred: progress.bytes_transferred,
            download_count: progress.download_count,
            upload_count: progress.upload_count,
        })
    }

    fn execute_upload(
        items: &Arc<Mutex<Vec<TransferItem>>>,
        transfer_progress: &Arc<Mutex<TransferProgressState>>,
        id: usize,
        session_info: &SftpSessionInfo,
        local_path: &str,
        remote_path: &str,
        is_dir: bool,
    ) -> Result<()> {
        let mut attempt = 0;

        info!("Starting upload #{}: {} -> {}", id, local_path, remote_path);

        loop {
            attempt += 1;

            if attempt > 1 {
                warn!("Upload #{} retry attempt {}", id, attempt);
            }

            {
                let mut items_guard = items.lock();
                if let Some(item) = items_guard.iter_mut().find(|i| i.id == id) {
                    if attempt > 1 {
                        item.status = TransferStatus::Retrying {
                            attempt,
                            reason: "Reconnecting...".to_string(),
                        };
                    } else {
                        item.status = TransferStatus::InProgress {
                            bytes_transferred: 0,
                            total_bytes: 0,
                        };
                    }
                }
            }

            let session = match Self::create_session(session_info) {
                Ok(s) => s,
                Err(e) => {
                    if attempt >= MAX_RETRIES {
                        return Err(e);
                    }
                    thread::sleep(std::time::Duration::from_millis(
                        RETRY_DELAY_MS * attempt as u64,
                    ));
                    continue;
                }
            };

            let sftp = session.sftp().context("Failed to create SFTP session")?;

            let result = if is_dir {
                Self::upload_directory(items, transfer_progress, id, &sftp, local_path, remote_path)
            } else {
                Self::upload_file(items, transfer_progress, id, &sftp, local_path, remote_path)
            };

            match result {
                Ok(()) => {
                    info!("Upload #{} completed successfully: {}", id, remote_path);
                    Self::mark_transfer_completed(items, transfer_progress, id);
                    return Ok(());
                }
                Err(e) => {
                    if attempt >= MAX_RETRIES {
                        error!("Upload #{} failed after {} attempts: {}", id, attempt, e);
                        return Err(e);
                    }
                    warn!("Upload #{} attempt {} failed: {}", id, attempt, e);
                    {
                        let mut items_guard = items.lock();
                        if let Some(item) = items_guard.iter_mut().find(|i| i.id == id) {
                            item.status = TransferStatus::Retrying {
                                attempt,
                                reason: e.to_string(),
                            };
                        }
                    }
                    thread::sleep(std::time::Duration::from_millis(
                        RETRY_DELAY_MS * attempt as u64,
                    ));
                }
            }
        }
    }

    fn upload_file(
        items: &Arc<Mutex<Vec<TransferItem>>>,
        transfer_progress: &Arc<Mutex<TransferProgressState>>,
        id: usize,
        sftp: &Sftp,
        local_path: &str,
        remote_path: &str,
    ) -> Result<()> {
        let local_path = Path::new(local_path);
        let remote_path = Path::new(remote_path);

        // Create temp remote path
        let temp_remote = remote_path.with_file_name(format!(
            "$.{}.temp",
            remote_path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
        ));

        // Get local file size
        let metadata = fs::metadata(local_path)?;
        let total_bytes = metadata.len();

        Self::update_transfer_progress(
            items,
            transfer_progress,
            id,
            total_bytes,
            0,
            TransferStatus::InProgress {
                bytes_transferred: 0,
                total_bytes,
            },
        );

        // Check for existing temp file to resume
        let resume_position = sftp
            .stat(&temp_remote)
            .ok()
            .and_then(|s| s.size)
            .unwrap_or(0);

        // Open local file
        let mut local_file = File::open(local_path)?;
        if resume_position > 0 {
            local_file.seek(SeekFrom::Start(resume_position))?;
        }

        // Open remote temp file
        let mut remote_file = if resume_position > 0 {
            sftp.open_mode(
                &temp_remote,
                ssh2::OpenFlags::WRITE | ssh2::OpenFlags::APPEND,
                0o644,
                ssh2::OpenType::File,
            )?
        } else {
            sftp.create(&temp_remote)?
        };

        let mut bytes_transferred = resume_position;
        Self::update_transfer_progress(
            items,
            transfer_progress,
            id,
            total_bytes,
            bytes_transferred,
            TransferStatus::InProgress {
                bytes_transferred,
                total_bytes,
            },
        );
        let mut buffer = vec![0u8; CHUNK_SIZE];

        loop {
            let bytes_read = local_file.read(&mut buffer)?;
            if bytes_read == 0 {
                break;
            }

            remote_file.write_all(&buffer[..bytes_read])?;
            bytes_transferred += bytes_read as u64;

            Self::update_transfer_progress(
                items,
                transfer_progress,
                id,
                total_bytes,
                bytes_transferred,
                TransferStatus::InProgress {
                    bytes_transferred,
                    total_bytes,
                },
            );
        }

        drop(remote_file);

        // Delete existing file if it exists, then rename temp to final
        // SFTP rename doesn't overwrite on some servers
        let _ = sftp.unlink(remote_path);

        sftp.rename(&temp_remote, remote_path, None)
            .context("Failed to rename temp file on remote")?;

        Ok(())
    }

    fn upload_directory(
        items: &Arc<Mutex<Vec<TransferItem>>>,
        transfer_progress: &Arc<Mutex<TransferProgressState>>,
        id: usize,
        sftp: &Sftp,
        local_path: &str,
        remote_path: &str,
    ) -> Result<()> {
        let local_path = Path::new(local_path);
        let remote_path = Path::new(remote_path);

        // Create remote directory
        let _ = sftp.mkdir(remote_path, 0o755);

        let entries = fs::read_dir(local_path)?;

        for entry in entries.flatten() {
            let filename = entry.file_name().to_string_lossy().to_string();
            let local_entry = entry.path().to_string_lossy().to_string();
            let remote_entry = remote_path.join(&filename).to_string_lossy().to_string();

            if entry.file_type()?.is_dir() {
                Self::upload_directory(
                    items,
                    transfer_progress,
                    id,
                    sftp,
                    &local_entry,
                    &remote_entry,
                )?;
            } else {
                Self::upload_file(
                    items,
                    transfer_progress,
                    id,
                    sftp,
                    &local_entry,
                    &remote_entry,
                )?;
            }
        }

        Ok(())
    }

    fn create_session(info: &SftpSessionInfo) -> Result<Session> {
        let tcp = TcpStream::connect(format!("{}:{}", info.host, info.port))
            .context("Failed to connect")?;

        let mut session = Session::new().context("Failed to create session")?;
        session.set_tcp_stream(tcp);
        session.handshake().context("Handshake failed")?;

        // Try agent authentication
        if session.userauth_agent(&info.username).is_ok() && session.authenticated() {
            return Ok(session);
        }

        // Try password authentication if provided
        if let Some(ref password) = info.password {
            if session.userauth_password(&info.username, password).is_ok()
                && session.authenticated()
            {
                return Ok(session);
            }
        }

        // Try key file if provided
        if let Some(ref key_path) = info.key_path {
            let key = PathBuf::from(key_path);
            if key.exists() {
                if session
                    .userauth_pubkey_file(&info.username, None, &key, None)
                    .is_ok()
                {
                    return Ok(session);
                }
            }
        }

        // Try default keys
        if let Some(home) = dirs::home_dir() {
            let ssh_dir = home.join(".ssh");
            for key_name in &["id_ed25519", "id_rsa", "id_ecdsa"] {
                let key_path = ssh_dir.join(key_name);
                if key_path.exists() {
                    if session
                        .userauth_pubkey_file(&info.username, None, &key_path, None)
                        .is_ok()
                    {
                        return Ok(session);
                    }
                }
            }
        }

        anyhow::bail!("Authentication failed")
    }

    pub fn clear_completed(&self) {
        let mut items = self.items.lock();
        items.retain(|item| {
            !matches!(
                item.status,
                TransferStatus::Completed | TransferStatus::Failed(_)
            )
        });
        drop(items);

        let has_downloads = self.items.lock().iter().any(|item| {
            matches!(
                item.status,
                TransferStatus::Pending
                    | TransferStatus::InProgress { .. }
                    | TransferStatus::Retrying { .. }
            )
        });
        if !has_downloads {
            *self.transfer_progress.lock() = TransferProgressState::default();
        }
    }
}

#[derive(Debug, Clone)]
pub struct SftpSessionInfo {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password: Option<String>,
    pub key_path: Option<String>,
}

pub fn create_zip(files: Vec<PathBuf>, base_dir: &Path, output_path: &Path) -> Result<()> {
    use std::io::BufWriter;
    use zip::write::SimpleFileOptions;
    use zip::ZipWriter;

    let file = File::create(output_path)?;
    let writer = BufWriter::new(file);
    let mut zip = ZipWriter::new(writer);

    let options = SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated)
        .unix_permissions(0o755);

    let mut added_dirs: HashSet<PathBuf> = HashSet::new();

    for path in files {
        let relative = path.strip_prefix(base_dir).unwrap_or(&path);

        if path.is_dir() {
            add_directory_to_zip(&mut zip, &path, relative, &options, &mut added_dirs)?;
        } else {
            // Ensure parent directories exist in zip
            if let Some(parent) = relative.parent() {
                add_parent_dirs(&mut zip, parent, &options, &mut added_dirs)?;
            }

            zip.start_file(relative.to_string_lossy(), options)?;
            let mut f = File::open(&path)?;
            std::io::copy(&mut f, &mut zip)?;
        }
    }

    zip.finish()?;
    Ok(())
}

#[derive(Debug, Clone)]
pub enum ExtractConflictStrategy {
    Overwrite,
    KeepBoth { timestamp: String },
}

pub fn list_zip_entries(archive_path: &Path) -> Result<Vec<PathBuf>> {
    use zip::ZipArchive;

    let file = File::open(archive_path)
        .with_context(|| format!("Failed to open {}", archive_path.display()))?;
    let mut archive = ZipArchive::new(file)
        .with_context(|| format!("Failed to read {}", archive_path.display()))?;

    let mut entries = Vec::new();

    for i in 0..archive.len() {
        let entry = archive.by_index(i).with_context(|| {
            format!(
                "Failed to read archive entry {} from {}",
                i,
                archive_path.display()
            )
        })?;

        let Some(relative_path) = entry.enclosed_name().map(|p| p.to_path_buf()) else {
            warn!(
                "Skipping unsafe zip entry in {}: {}",
                archive_path.display(),
                entry.name()
            );
            continue;
        };

        if !entry.is_dir() {
            entries.push(relative_path);
        }
    }

    Ok(entries)
}

pub fn extract_zip_archive(
    archive_path: &Path,
    destination: &Path,
    strategy: &ExtractConflictStrategy,
) -> Result<()> {
    use zip::ZipArchive;

    let file = File::open(archive_path)
        .with_context(|| format!("Failed to open {}", archive_path.display()))?;
    let mut archive = ZipArchive::new(file)
        .with_context(|| format!("Failed to read {}", archive_path.display()))?;
    let mut reserved_paths: HashSet<PathBuf> = HashSet::new();

    for i in 0..archive.len() {
        let mut entry = archive.by_index(i).with_context(|| {
            format!(
                "Failed to read archive entry {} from {}",
                i,
                archive_path.display()
            )
        })?;

        let Some(relative_path) = entry.enclosed_name().map(|p| p.to_path_buf()) else {
            warn!(
                "Skipping unsafe zip entry in {}: {}",
                archive_path.display(),
                entry.name()
            );
            continue;
        };

        let original_output_path = destination.join(relative_path);

        if entry.is_dir() {
            fs::create_dir_all(&original_output_path)?;
            reserved_paths.insert(original_output_path);
            continue;
        }

        let output_path = match strategy {
            ExtractConflictStrategy::Overwrite => original_output_path,
            ExtractConflictStrategy::KeepBoth { timestamp } => {
                if original_output_path.exists() || reserved_paths.contains(&original_output_path) {
                    resolve_keep_both_path(&original_output_path, timestamp, &reserved_paths)
                } else {
                    original_output_path
                }
            }
        };

        if let Some(parent) = output_path.parent() {
            fs::create_dir_all(parent)?;
        }

        let mut output = File::create(&output_path)?;
        std::io::copy(&mut entry, &mut output)?;
        reserved_paths.insert(output_path.clone());

        #[cfg(unix)]
        if let Some(mode) = entry.unix_mode() {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&output_path, fs::Permissions::from_mode(mode))?;
        }
    }

    Ok(())
}

fn resolve_keep_both_path(
    original_path: &Path,
    timestamp: &str,
    reserved_paths: &HashSet<PathBuf>,
) -> PathBuf {
    let parent = original_path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_default();
    let file_name = original_path
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| "file".to_string());

    let mut candidate = parent.join(insert_timestamp_before_extension(&file_name, timestamp));
    let mut suffix = 2usize;

    while candidate.exists() || reserved_paths.contains(&candidate) {
        candidate = parent.join(insert_timestamp_before_extension(
            &file_name,
            &format!("{}.{}", timestamp, suffix),
        ));
        suffix += 1;
    }

    candidate
}

fn insert_timestamp_before_extension(file_name: &str, timestamp: &str) -> String {
    if let Some((stem, ext)) = file_name.rsplit_once('.') {
        if !stem.is_empty() {
            return format!("{}.{}.{}", stem, timestamp, ext);
        }
    }

    format!("{}.{}", file_name, timestamp)
}

fn add_parent_dirs<W: Write + Seek>(
    zip: &mut zip::ZipWriter<W>,
    path: &Path,
    options: &zip::write::SimpleFileOptions,
    added: &mut HashSet<PathBuf>,
) -> Result<()> {
    let mut current = PathBuf::new();
    for component in path.components() {
        current.push(component);
        if !added.contains(&current) {
            let dir_name = format!("{}/", current.to_string_lossy());
            zip.add_directory(&dir_name, *options)?;
            added.insert(current.clone());
        }
    }
    Ok(())
}

fn add_directory_to_zip<W: Write + Seek>(
    zip: &mut zip::ZipWriter<W>,
    dir_path: &Path,
    relative_base: &Path,
    options: &zip::write::SimpleFileOptions,
    added: &mut HashSet<PathBuf>,
) -> Result<()> {
    use walkdir::WalkDir;

    for entry in WalkDir::new(dir_path).into_iter().filter_map(|e| e.ok()) {
        let path = entry.path();
        let relative = relative_base.join(path.strip_prefix(dir_path).unwrap_or(path));

        if path.is_dir() {
            if !added.contains(&relative) {
                let dir_name = format!("{}/", relative.to_string_lossy());
                zip.add_directory(&dir_name, *options)?;
                added.insert(relative);
            }
        } else {
            if let Some(parent) = relative.parent() {
                add_parent_dirs(zip, parent, options, added)?;
            }
            zip.start_file(relative.to_string_lossy(), *options)?;
            let mut f = File::open(path)?;
            std::io::copy(&mut f, zip)?;
        }
    }

    Ok(())
}
