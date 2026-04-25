use crate::db::{AuthMethod, Database, SavedConnection};
use crate::editor::{detect_default_editor, EditorManager};
use crate::ssh::{ConnectionParams, SshConnection};
use crate::terminal::{LocalTerminal, RemoteTerminal};
use crate::transfer::{
    create_zip, extract_zip_archive, list_zip_entries, ExtractConflictStrategy, SftpSessionInfo,
    TransferManager, TransferStatus,
};
use anyhow::Result;
use log::{debug, error, info, warn};
use serde::{Deserialize, Serialize};
use ssh2::Sftp;
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuTab {
    File,
    Connect,
    Help,
}

/// Rows in File → Settings (used for keyboard navigation).
pub const SETTINGS_ROW_COUNT: usize = 10;
pub const SETTINGS_ROW_EDITOR: usize = 7;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum ExplorerSortField {
    Name,
    Size,
    Permissions,
    Modified,
    Created,
}

impl Default for ExplorerSortField {
    fn default() -> Self {
        Self::Name
    }
}

impl ExplorerSortField {
    pub fn next(self) -> Self {
        match self {
            Self::Name => Self::Size,
            Self::Size => Self::Permissions,
            Self::Permissions => Self::Modified,
            Self::Modified => Self::Created,
            Self::Created => Self::Name,
        }
    }

    pub fn prev(self) -> Self {
        match self {
            Self::Name => Self::Created,
            Self::Size => Self::Name,
            Self::Permissions => Self::Size,
            Self::Modified => Self::Permissions,
            Self::Created => Self::Modified,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Name => "Name",
            Self::Size => "Size",
            Self::Permissions => "Permissions",
            Self::Modified => "Last modified",
            Self::Created => "Created",
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum ExplorerSortDirection {
    Ascending,
    Descending,
}

impl Default for ExplorerSortDirection {
    fn default() -> Self {
        Self::Ascending
    }
}

impl ExplorerSortDirection {
    pub fn next(self) -> Self {
        match self {
            Self::Ascending => Self::Descending,
            Self::Descending => Self::Ascending,
        }
    }

    pub fn prev(self) -> Self {
        self.next()
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Ascending => "Ascending",
            Self::Descending => "Descending",
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct ExplorerSort {
    pub field: ExplorerSortField,
    pub direction: ExplorerSortDirection,
}

impl Default for ExplorerSort {
    fn default() -> Self {
        Self {
            field: ExplorerSortField::Name,
            direction: ExplorerSortDirection::Ascending,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(default)]
pub struct ExplorerColumns {
    pub show_headers: bool,
    pub show_size: bool,
    pub show_permissions: bool,
    pub show_modified: bool,
    pub show_created: bool,
}

impl Default for ExplorerColumns {
    fn default() -> Self {
        Self {
            show_headers: true,
            show_size: true,
            show_permissions: true,
            show_modified: true,
            show_created: false,
        }
    }
}

#[derive(Debug, Clone)]
pub struct UserPreferences {
    pub explorer_columns: ExplorerColumns,
    pub explorer_sort: ExplorerSort,
    pub editor_command: String,
    /// When true, new or synced terminals start in the same directory as the file explorer.
    pub open_terminal_in_explorer_dir: bool,
    /// When true, the local file explorer tracks `cd` in the local terminal (local panel + local PTY only).
    pub explorer_follows_terminal: bool,
}

impl UserPreferences {
    pub fn load_from_db(db: &Database, default_editor: &str) -> Result<Self> {
        let open_terminal_in_explorer_dir = db
            .get_setting("open_terminal_in_explorer_dir")?
            .as_deref()
            .map(parse_bool_pref)
            .unwrap_or(true);
        let explorer_follows_terminal = db
            .get_setting("explorer_follows_terminal")?
            .as_deref()
            .map(parse_bool_pref)
            .unwrap_or(false);
        let editor_command = db
            .get_setting("editor_command")?
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| default_editor.to_string());
        let explorer_columns = db
            .get_setting("explorer_columns")?
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        let explorer_sort = db
            .get_setting("explorer_sort")?
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        Ok(Self {
            explorer_columns,
            explorer_sort,
            editor_command,
            open_terminal_in_explorer_dir,
            explorer_follows_terminal,
        })
    }

    pub fn save_to_db(&self, db: &Database) -> Result<()> {
        db.set_setting(
            "open_terminal_in_explorer_dir",
            if self.open_terminal_in_explorer_dir {
                "1"
            } else {
                "0"
            },
        )?;
        db.set_setting(
            "explorer_follows_terminal",
            if self.explorer_follows_terminal {
                "1"
            } else {
                "0"
            },
        )?;
        db.set_setting("editor_command", &self.editor_command)?;
        db.set_setting(
            "explorer_columns",
            &serde_json::to_string(&self.explorer_columns)?,
        )?;
        db.set_setting(
            "explorer_sort",
            &serde_json::to_string(&self.explorer_sort)?,
        )?;
        Ok(())
    }
}

fn parse_bool_pref(s: &str) -> bool {
    matches!(s.to_lowercase().as_str(), "1" | "true" | "yes" | "on")
}

#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileMenuItem {
    Exit,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectMenuItem {
    NewConnection,
    RecentConnections,
    ShowAllConnections,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DialogField {
    Name,
    Host,
    Port,
    Username,
    Password,
    KeyPath,
}

impl DialogField {
    pub fn next(self) -> Self {
        match self {
            Self::Name => Self::Host,
            Self::Host => Self::Port,
            Self::Port => Self::Username,
            Self::Username => Self::Password,
            Self::Password => Self::KeyPath,
            Self::KeyPath => Self::Name,
        }
    }

    pub fn prev(self) -> Self {
        match self {
            Self::Name => Self::KeyPath,
            Self::Host => Self::Name,
            Self::Port => Self::Host,
            Self::Username => Self::Port,
            Self::Password => Self::Username,
            Self::KeyPath => Self::Password,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct ConnectionDialog {
    pub name: String,
    pub host: String,
    pub port: String,
    pub username: String,
    pub password: String,
    pub key_path: String,
    pub active_field: DialogField,
    pub error_message: Option<String>,
}

impl Default for DialogField {
    fn default() -> Self {
        Self::Name
    }
}

impl ConnectionDialog {
    pub fn new() -> Self {
        Self {
            port: "22".to_string(),
            ..Default::default()
        }
    }

    pub fn get_field_value(&self, field: DialogField) -> &str {
        match field {
            DialogField::Name => &self.name,
            DialogField::Host => &self.host,
            DialogField::Port => &self.port,
            DialogField::Username => &self.username,
            DialogField::Password => &self.password,
            DialogField::KeyPath => &self.key_path,
        }
    }

    pub fn get_field_value_mut(&mut self, field: DialogField) -> &mut String {
        match field {
            DialogField::Name => &mut self.name,
            DialogField::Host => &mut self.host,
            DialogField::Port => &mut self.port,
            DialogField::Username => &mut self.username,
            DialogField::Password => &mut self.password,
            DialogField::KeyPath => &mut self.key_path,
        }
    }

    pub fn to_connection_params(&self) -> Result<ConnectionParams> {
        let port: u16 = self
            .port
            .parse()
            .map_err(|_| anyhow::anyhow!("Invalid port number"))?;

        if self.host.is_empty() {
            anyhow::bail!("Host is required");
        }

        if self.username.is_empty() {
            anyhow::bail!("Username is required");
        }

        Ok(ConnectionParams {
            host: self.host.clone(),
            port,
            username: self.username.clone(),
            password: if self.password.is_empty() {
                None
            } else {
                Some(self.password.clone())
            },
            key_path: if self.key_path.is_empty() {
                None
            } else {
                Some(self.key_path.clone())
            },
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppMode {
    Normal,
    MenuFocused,
    MenuOpen,
    ConnectionDialog,
    ConnectionList,
    Connected,
    DirectoryInput,
    RenameInput,
    DeleteConfirm,
    ExtractConflictConfirm,
    Settings,
    KeyboardShortcuts,
}

#[derive(Debug, Clone)]
pub struct PendingExtractOperation {
    pub target: FocusPanel,
    pub current_dir: String,
    pub archives: Vec<String>,
    pub conflict_paths: Vec<String>,
}

#[derive(Debug, Clone)]
pub enum PendingDeleteOperation {
    Files(Vec<(String, bool)>),
    SavedConnection(SavedConnection),
}

#[derive(Debug, Clone)]
struct DownloadPlanItem {
    remote_path: String,
    dest_path: String,
    total_bytes: u64,
}

#[derive(Debug, Clone)]
struct UploadPlanItem {
    local_path: String,
    remote_path: String,
    total_bytes: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FocusPanel {
    Local,
    Remote,
    /// Bottom connection tab strip (keyboard selection); only used when `tabs.len() > 1`.
    ConnectionTabs,
}

#[derive(Debug, Clone)]
pub struct FileEntry {
    pub name: String,
    pub is_dir: bool,
    pub size: String,
    pub size_bytes: Option<u64>,
    pub permissions: String,
    pub modified: String,
    pub created: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilterMode {
    None,
    Normal,
    Regex,
}

pub struct FileBrowser {
    pub current_dir: String,
    pub files: Vec<FileEntry>,
    pub sort: ExplorerSort,
    pub selected_index: usize,
    pub scroll_offset: usize,
    pub selected_indices: HashSet<usize>,
    pub shift_selecting: bool,
    pub shift_anchor: Option<usize>,
    pub filter: String,
    pub filter_mode: FilterMode,
}

impl FileBrowser {
    pub fn new(start_dir: String, sort: ExplorerSort) -> Self {
        Self {
            current_dir: start_dir,
            files: Vec::new(),
            sort,
            selected_index: 0,
            scroll_offset: 0,
            selected_indices: HashSet::new(),
            shift_selecting: false,
            shift_anchor: None,
            filter: String::new(),
            filter_mode: FilterMode::None,
        }
    }

    pub fn sort_files(&mut self) {
        let selected_name = self.selected_file().map(|file| file.name.clone());
        let selected_names: HashSet<String> = self
            .selected_indices
            .iter()
            .filter_map(|&idx| self.files.get(idx).map(|file| file.name.clone()))
            .collect();

        sort_file_entries(&mut self.files, self.sort);

        self.selected_indices = self
            .files
            .iter()
            .enumerate()
            .filter(|(_, file)| selected_names.contains(&file.name))
            .map(|(idx, _)| idx)
            .collect();

        let filtered_len = self.filtered_len();
        if let Some(name) = selected_name {
            if self.filter.is_empty() {
                if let Some(idx) = self.files.iter().position(|file| file.name == name) {
                    self.selected_index = idx;
                } else {
                    self.selected_index =
                        self.selected_index.min(self.files.len().saturating_sub(1));
                }
            } else {
                if let Some((idx, _)) = self
                    .filtered_files()
                    .iter()
                    .enumerate()
                    .find(|(_, (_, file))| file.name == name)
                {
                    self.selected_index = idx;
                } else {
                    self.selected_index = self.selected_index.min(filtered_len.saturating_sub(1));
                }
            }
        } else {
            self.selected_index = self.selected_index.min(filtered_len.saturating_sub(1));
        }
    }

    pub fn start_filter(&mut self, mode: FilterMode) {
        self.filter_mode = mode;
        self.filter.clear();
        self.selected_index = 0;
        self.scroll_offset = 0;
    }

    pub fn is_filtering(&self) -> bool {
        self.filter_mode != FilterMode::None
    }

    pub fn filtered_files(&self) -> Vec<(usize, &FileEntry)> {
        if self.filter.is_empty() || self.filter_mode == FilterMode::None {
            self.files.iter().enumerate().collect()
        } else {
            match self.filter_mode {
                FilterMode::None => self.files.iter().enumerate().collect(),
                FilterMode::Normal => {
                    let filter_lower = self.filter.to_lowercase();
                    self.files
                        .iter()
                        .enumerate()
                        .filter(|(_, f)| f.name.to_lowercase().contains(&filter_lower))
                        .collect()
                }
                FilterMode::Regex => {
                    if let Ok(re) = regex::Regex::new(&self.filter) {
                        self.files
                            .iter()
                            .enumerate()
                            .filter(|(_, f)| re.is_match(&f.name))
                            .collect()
                    } else {
                        self.files.iter().enumerate().collect()
                    }
                }
            }
        }
    }

    pub fn add_filter_char(&mut self, c: char) {
        self.filter.push(c);
        self.selected_index = 0;
        self.scroll_offset = 0;
    }

    pub fn remove_filter_char(&mut self) {
        self.filter.pop();
    }

    pub fn clear_filter(&mut self) {
        self.filter.clear();
        self.filter_mode = FilterMode::None;
        self.selected_index = 0;
        self.scroll_offset = 0;
    }

    fn filtered_len(&self) -> usize {
        self.filtered_files().len()
    }

    pub fn selected_file(&self) -> Option<&FileEntry> {
        if self.filter.is_empty() {
            self.files.get(self.selected_index)
        } else {
            self.filtered_files()
                .get(self.selected_index)
                .map(|(_, file)| *file)
        }
    }

    pub fn selected_original_index(&self) -> Option<usize> {
        if self.filter.is_empty() {
            Some(self.selected_index)
        } else {
            self.filtered_files()
                .get(self.selected_index)
                .map(|(orig_idx, _)| *orig_idx)
        }
    }

    pub fn move_up(&mut self) {
        if self.selected_index > 0 {
            self.selected_index -= 1;
        }
        self.shift_selecting = false;
        self.shift_anchor = None;
    }

    pub fn move_down(&mut self) {
        let max_idx = if self.filter.is_empty() {
            self.files.len().saturating_sub(1)
        } else {
            self.filtered_len().saturating_sub(1)
        };
        if self.selected_index < max_idx {
            self.selected_index += 1;
        }
        self.shift_selecting = false;
        self.shift_anchor = None;
    }

    pub fn page_up(&mut self, page_size: usize) {
        self.selected_index = self.selected_index.saturating_sub(page_size);
        self.shift_selecting = false;
        self.shift_anchor = None;
    }

    pub fn page_down(&mut self, page_size: usize) {
        let max_idx = if self.filter.is_empty() {
            self.files.len().saturating_sub(1)
        } else {
            self.filtered_len().saturating_sub(1)
        };
        self.selected_index = (self.selected_index + page_size).min(max_idx);
        self.shift_selecting = false;
        self.shift_anchor = None;
    }

    pub fn move_up_shift(&mut self) {
        if self.selected_index == 0 {
            return;
        }

        let orig_idx = self.selected_original_index();

        if !self.shift_selecting {
            self.shift_selecting = true;
            self.shift_anchor = orig_idx;
            if let Some(idx) = orig_idx {
                self.selected_indices.insert(idx);
            }
        }

        self.selected_index -= 1;

        let new_orig_idx = self.selected_original_index();

        if let (Some(anchor), Some(new_idx)) = (self.shift_anchor, new_orig_idx) {
            if new_idx < anchor {
                self.selected_indices.insert(new_idx);
            } else if let Some(prev_orig) = orig_idx {
                self.selected_indices.remove(&prev_orig);
            }
        }
    }

    pub fn move_down_shift(&mut self) {
        let max_idx = if self.filter.is_empty() {
            self.files.len().saturating_sub(1)
        } else {
            self.filtered_len().saturating_sub(1)
        };

        if self.selected_index >= max_idx {
            return;
        }

        let orig_idx = self.selected_original_index();

        if !self.shift_selecting {
            self.shift_selecting = true;
            self.shift_anchor = orig_idx;
            if let Some(idx) = orig_idx {
                self.selected_indices.insert(idx);
            }
        }

        self.selected_index += 1;

        let new_orig_idx = self.selected_original_index();

        if let (Some(anchor), Some(new_idx)) = (self.shift_anchor, new_orig_idx) {
            if new_idx > anchor {
                self.selected_indices.insert(new_idx);
            } else if let Some(prev_orig) = orig_idx {
                self.selected_indices.remove(&prev_orig);
            }
        }
    }

    pub fn toggle_select_current(&mut self) {
        if let Some(orig_idx) = self.selected_original_index() {
            if self.selected_indices.contains(&orig_idx) {
                self.selected_indices.remove(&orig_idx);
            } else {
                self.selected_indices.insert(orig_idx);
            }
        }
        self.shift_selecting = false;
        self.shift_anchor = None;
    }

    pub fn clear_selection(&mut self) {
        self.selected_indices.clear();
        self.shift_selecting = false;
        self.shift_anchor = None;
    }

    pub fn get_selected_files(&self) -> Vec<&FileEntry> {
        if self.selected_indices.is_empty() {
            self.selected_file().into_iter().collect()
        } else {
            self.selected_indices
                .iter()
                .filter_map(|&i| self.files.get(i))
                .collect()
        }
    }

    /// Single entry to rename: cursor, or the one marked row when exactly one `selected_indices` entry.
    pub fn rename_target_entry(&self) -> Option<&FileEntry> {
        if self.selected_indices.len() > 1 {
            return None;
        }
        if self.selected_indices.len() == 1 {
            let idx = *self.selected_indices.iter().next()?;
            return self.files.get(idx);
        }
        self.selected_file()
    }

    pub fn is_selected(&self, index: usize) -> bool {
        self.selected_indices.contains(&index)
    }

    #[allow(dead_code)]
    pub fn adjust_scroll(&mut self, visible_height: usize) {
        if self.selected_index < self.scroll_offset {
            self.scroll_offset = self.selected_index;
        } else if self.selected_index >= self.scroll_offset + visible_height {
            self.scroll_offset = self.selected_index - visible_height + 1;
        }
    }
}

fn compare_file_entries(a: &FileEntry, b: &FileEntry, sort: ExplorerSort) -> std::cmp::Ordering {
    if a.name == ".." && b.name != ".." {
        return std::cmp::Ordering::Less;
    }
    if a.name != ".." && b.name == ".." {
        return std::cmp::Ordering::Greater;
    }

    let primary = match sort.field {
        ExplorerSortField::Name => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
        ExplorerSortField::Size => a.size_bytes.cmp(&b.size_bytes),
        ExplorerSortField::Permissions => a.permissions.cmp(&b.permissions),
        ExplorerSortField::Modified => a.modified.cmp(&b.modified),
        ExplorerSortField::Created => a.created.as_deref().cmp(&b.created.as_deref()),
    };

    let ordered = match sort.direction {
        ExplorerSortDirection::Ascending => primary,
        ExplorerSortDirection::Descending => primary.reverse(),
    };

    ordered.then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
}

fn sort_file_entries(files: &mut [FileEntry], sort: ExplorerSort) {
    files.sort_by(|a, b| compare_file_entries(a, b, sort));
}

pub struct LocalBrowser {
    pub browser: FileBrowser,
}

impl LocalBrowser {
    pub fn new(sort: ExplorerSort) -> Result<Self> {
        let current_dir = std::env::current_dir()?.to_string_lossy().to_string();

        let mut browser = FileBrowser::new(current_dir, sort);
        Self::refresh_files(&mut browser)?;

        Ok(Self { browser })
    }

    pub fn refresh_files(browser: &mut FileBrowser) -> Result<()> {
        let prev_index = browser.selected_index;
        browser.files.clear();
        browser.clear_selection();

        browser.files.push(FileEntry {
            name: "..".to_string(),
            is_dir: true,
            size: "-".to_string(),
            size_bytes: None,
            permissions: "drwxr-xr-x".to_string(),
            modified: "".to_string(),
            created: None,
        });

        let entries = std::fs::read_dir(&browser.current_dir)?;

        for entry in entries.flatten() {
            let metadata = entry.metadata()?;
            let name = entry.file_name().to_string_lossy().to_string();

            if name.starts_with('.') {
                continue;
            }

            let is_dir = metadata.is_dir();
            let size = if is_dir {
                "-".to_string()
            } else {
                Self::format_size(metadata.len())
            };
            let size_bytes = if is_dir { None } else { Some(metadata.len()) };

            let permissions = Self::format_permissions(&metadata);
            let modified = Self::format_modified(&metadata);
            let created = Self::format_created(&metadata);

            browser.files.push(FileEntry {
                name,
                is_dir,
                size,
                size_bytes,
                permissions,
                modified,
                created,
            });
        }

        browser.sort_files();

        browser.selected_index = prev_index.min(browser.files.len().saturating_sub(1));

        Ok(())
    }

    pub fn refresh_files_reset_index(browser: &mut FileBrowser) -> Result<()> {
        browser.files.clear();
        browser.clear_selection();

        browser.files.push(FileEntry {
            name: "..".to_string(),
            is_dir: true,
            size: "-".to_string(),
            size_bytes: None,
            permissions: "drwxr-xr-x".to_string(),
            modified: "".to_string(),
            created: None,
        });

        let entries = std::fs::read_dir(&browser.current_dir)?;

        for entry in entries.flatten() {
            let metadata = entry.metadata()?;
            let name = entry.file_name().to_string_lossy().to_string();

            if name.starts_with('.') {
                continue;
            }

            let is_dir = metadata.is_dir();
            let size = if is_dir {
                "-".to_string()
            } else {
                Self::format_size(metadata.len())
            };
            let size_bytes = if is_dir { None } else { Some(metadata.len()) };

            let permissions = Self::format_permissions(&metadata);
            let modified = Self::format_modified(&metadata);
            let created = Self::format_created(&metadata);

            browser.files.push(FileEntry {
                name,
                is_dir,
                size,
                size_bytes,
                permissions,
                modified,
                created,
            });
        }

        browser.sort_files();

        browser.selected_index = 0;
        browser.scroll_offset = 0;

        Ok(())
    }

    fn format_size(bytes: u64) -> String {
        const KB: u64 = 1024;
        const MB: u64 = KB * 1024;
        const GB: u64 = MB * 1024;
        const TB: u64 = GB * 1024;

        if bytes >= TB {
            format!("{:.1}T", bytes as f64 / TB as f64)
        } else if bytes >= GB {
            format!("{:.1}G", bytes as f64 / GB as f64)
        } else if bytes >= MB {
            format!("{:.1}M", bytes as f64 / MB as f64)
        } else if bytes >= KB {
            format!("{:.1}K", bytes as f64 / KB as f64)
        } else {
            format!("{}B", bytes)
        }
    }

    fn format_permissions(metadata: &std::fs::Metadata) -> String {
        use std::os::unix::fs::PermissionsExt;
        let mode = metadata.permissions().mode();
        let file_type = if metadata.is_dir() { 'd' } else { '-' };

        let perms = [
            if mode & 0o400 != 0 { 'r' } else { '-' },
            if mode & 0o200 != 0 { 'w' } else { '-' },
            if mode & 0o100 != 0 { 'x' } else { '-' },
            if mode & 0o040 != 0 { 'r' } else { '-' },
            if mode & 0o020 != 0 { 'w' } else { '-' },
            if mode & 0o010 != 0 { 'x' } else { '-' },
            if mode & 0o004 != 0 { 'r' } else { '-' },
            if mode & 0o002 != 0 { 'w' } else { '-' },
            if mode & 0o001 != 0 { 'x' } else { '-' },
        ];

        format!("{}{}", file_type, perms.iter().collect::<String>())
    }

    fn format_modified(metadata: &std::fs::Metadata) -> String {
        if let Ok(modified) = metadata.modified() {
            let datetime: chrono::DateTime<chrono::Local> = modified.into();
            datetime.format("%Y-%m-%d %H:%M").to_string()
        } else {
            "".to_string()
        }
    }

    fn format_created(metadata: &std::fs::Metadata) -> Option<String> {
        metadata.created().ok().map(|created| {
            let datetime: chrono::DateTime<chrono::Local> = created.into();
            datetime.format("%Y-%m-%d %H:%M").to_string()
        })
    }

    pub fn change_directory(browser: &mut FileBrowser, path: &str) -> Result<()> {
        let new_dir = if path == ".." {
            std::path::Path::new(&browser.current_dir)
                .parent()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_else(|| "/".to_string())
        } else if path.starts_with('/') {
            path.to_string()
        } else if path.starts_with('~') {
            let home =
                dirs::home_dir().ok_or_else(|| anyhow::anyhow!("Could not find home directory"))?;
            if path == "~" {
                home.to_string_lossy().to_string()
            } else {
                home.join(&path[2..]).to_string_lossy().to_string()
            }
        } else {
            std::path::Path::new(&browser.current_dir)
                .join(path)
                .to_string_lossy()
                .to_string()
        };

        if !std::path::Path::new(&new_dir).is_dir() {
            anyhow::bail!("Not a directory: {}", new_dir);
        }

        browser.current_dir = new_dir;
        Self::refresh_files_reset_index(browser)?;

        Ok(())
    }

    #[allow(dead_code)]
    pub fn open_file(path: &str) -> Result<()> {
        #[cfg(target_os = "macos")]
        {
            Command::new("open").arg(path).spawn()?;
        }
        #[cfg(target_os = "linux")]
        {
            Command::new("xdg-open").arg(path).spawn()?;
        }
        Ok(())
    }

    #[allow(dead_code)]
    pub fn execute_command(current_dir: &str, command: &str) -> Result<Option<String>> {
        let output = Command::new("sh")
            .arg("-c")
            .arg(command)
            .current_dir(current_dir)
            .output()?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).to_string();
            if !stderr.is_empty() {
                return Ok(Some(stderr));
            }
        }

        Ok(None)
    }
}

pub struct ConnectionTab {
    #[allow(dead_code)]
    pub id: usize,
    pub name: String,
    pub connection: SshConnection,
    pub browser: FileBrowser,
    pub session_info: SftpSessionInfo,
    pub remote_terminal: Option<RemoteTerminal>,
    pub remote_terminal_visible: bool,
}

impl ConnectionTab {
    pub fn new(
        id: usize,
        name: String,
        connection: SshConnection,
        session_info: SftpSessionInfo,
        sort: ExplorerSort,
    ) -> Self {
        Self {
            id,
            name,
            connection,
            browser: FileBrowser::new(String::new(), sort),
            session_info,
            remote_terminal: None,
            remote_terminal_visible: false,
        }
    }

    pub fn refresh_directory(&mut self) -> Result<()> {
        if self.browser.current_dir.is_empty() {
            self.browser.current_dir = self.connection.get_remote_pwd()?;
        }

        let prev_index = self.browser.selected_index;
        let output = self
            .connection
            .exec(&format!("ls -la {}", &self.browser.current_dir))?;

        self.browser.files.clear();
        self.browser.clear_selection();

        for line in output.lines().skip(1) {
            if let Some(entry) = Self::parse_ls_line(line) {
                self.browser.files.push(entry);
            }
        }

        self.browser.sort_files();

        self.browser.selected_index = prev_index.min(self.browser.files.len().saturating_sub(1));

        Ok(())
    }

    pub fn refresh_directory_reset_index(&mut self) -> Result<()> {
        if self.browser.current_dir.is_empty() {
            self.browser.current_dir = self.connection.get_remote_pwd()?;
        }

        let output = self
            .connection
            .exec(&format!("ls -la {}", &self.browser.current_dir))?;

        self.browser.files.clear();
        self.browser.clear_selection();

        for line in output.lines().skip(1) {
            if let Some(entry) = Self::parse_ls_line(line) {
                self.browser.files.push(entry);
            }
        }

        self.browser.sort_files();

        self.browser.selected_index = 0;
        self.browser.scroll_offset = 0;

        Ok(())
    }

    fn parse_ls_line(line: &str) -> Option<FileEntry> {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() < 8 {
            return None;
        }

        let permissions = parts[0].to_string();

        if permissions == "total" || permissions.len() < 10 {
            return None;
        }

        let is_dir = permissions.starts_with('d');
        let size_bytes = parts.get(4).unwrap_or(&"0").parse::<u64>().unwrap_or(0);
        let size = if is_dir {
            "-".to_string()
        } else {
            Self::format_size_human(size_bytes)
        };

        let modified = format!(
            "{} {} {}",
            parts.get(5).unwrap_or(&""),
            parts.get(6).unwrap_or(&""),
            parts.get(7).unwrap_or(&"")
        );

        let name = if parts.len() > 8 {
            parts[8..].join(" ")
        } else if parts.len() == 8 {
            parts[7].to_string()
        } else {
            return None;
        };

        if name == "." {
            return None;
        }

        let name = if let Some(pos) = name.find(" -> ") {
            name[..pos].to_string()
        } else {
            name
        };

        Some(FileEntry {
            name,
            is_dir,
            size,
            size_bytes: if is_dir { None } else { Some(size_bytes) },
            permissions,
            modified,
            created: None,
        })
    }

    fn format_size_human(bytes: u64) -> String {
        const KB: u64 = 1024;
        const MB: u64 = KB * 1024;
        const GB: u64 = MB * 1024;
        const TB: u64 = GB * 1024;

        if bytes >= TB {
            format!("{:.1}T", bytes as f64 / TB as f64)
        } else if bytes >= GB {
            format!("{:.1}G", bytes as f64 / GB as f64)
        } else if bytes >= MB {
            format!("{:.1}M", bytes as f64 / MB as f64)
        } else if bytes >= KB {
            format!("{:.1}K", bytes as f64 / KB as f64)
        } else {
            format!("{}B", bytes)
        }
    }

    pub fn change_directory(&mut self, path: &str) -> Result<()> {
        let cd_command = if path.starts_with('/') || path.starts_with('~') {
            format!("cd {} && pwd", path)
        } else {
            format!("cd \"{}\" && cd {} && pwd", &self.browser.current_dir, path)
        };

        debug!("Remote cd command: {}", cd_command);

        let result = self.connection.exec(&cd_command)?;
        let new_path = result.trim().to_string();

        if new_path.is_empty() {
            anyhow::bail!("Failed to change directory to: {}", path);
        }

        self.browser.current_dir = new_path;
        self.refresh_directory_reset_index()?;

        Ok(())
    }

    #[allow(dead_code)]
    pub fn execute_command(&mut self, command: &str) -> Result<Option<String>> {
        let full_cmd = format!("cd {} && {} 2>&1", &self.browser.current_dir, command);
        debug!("Remote execute: {}", full_cmd);

        let output = self.connection.exec(&full_cmd)?;

        if !output.trim().is_empty() {
            info!("Remote command output:\n{}", output);
        }

        let trimmed = command.trim();
        if trimmed.starts_with("cd ") || trimmed == "cd" {
            let path = trimmed.strip_prefix("cd").unwrap_or("").trim();
            let path = if path.is_empty() { "~" } else { path };
            self.change_directory(path)?;
        } else if trimmed.contains("&&") || trimmed.contains(";") {
            let _ = self.refresh_directory();
        }

        let lower = output.to_lowercase();
        if lower.contains("error")
            || lower.contains("not found")
            || lower.contains("permission denied")
            || lower.contains("no such")
            || lower.contains("command not found")
            || lower.contains("cannot")
            || lower.contains("failed")
            || lower.contains("denied")
        {
            warn!("Remote command may have errors: {}", output.trim());
            Ok(Some(output.trim().to_string()))
        } else {
            Ok(None)
        }
    }
}

pub struct App {
    pub running: bool,
    pub mode: AppMode,
    pub focus: FocusPanel,
    pub active_menu_tab: MenuTab,
    pub file_menu_index: usize,
    pub connect_menu_index: usize,
    pub help_menu_index: usize,
    pub shortcuts_scroll_offset: usize,
    /// Viewport height (lines) of the keyboard shortcuts dialog; updated each frame while that dialog is open.
    pub shortcuts_viewport_height: usize,
    /// Line count in the shortcuts help text; updated each frame while that dialog is open.
    pub shortcuts_help_line_count: usize,
    pub connection_dialog: ConnectionDialog,
    pub db: Database,
    pub local: LocalBrowser,
    pub tabs: Vec<ConnectionTab>,
    pub active_tab: usize,
    /// Index highlighted when `focus == ConnectionTabs` (may differ from `active_tab` until Enter).
    pub tab_bar_highlight: usize,
    pub next_tab_id: usize,
    pub all_connections: Vec<SavedConnection>,
    pub recent_connections: Vec<SavedConnection>,
    pub connection_list_index: usize,
    pub showing_recent: bool,
    pub status_message: Option<String>,
    pub error_message: Option<String>,
    pub visible_file_rows: usize,
    /// Last rendered height of the local terminal panel (for page scroll / Ctrl+Y / Ctrl+V).
    pub visible_local_terminal_rows: usize,
    /// Last rendered height of the remote terminal panel.
    pub visible_remote_terminal_rows: usize,
    pub transfer_manager: TransferManager,
    pub editor_manager: EditorManager,
    pub directory_input: String,
    pub directory_completions: Vec<String>,
    pub directory_completion_index: usize,
    pub rename_input: String,
    pub rename_old_name: String,
    pub last_slash_press: Option<std::time::Instant>,
    /// First `Z` of a possible `Z Z` double-press; if no second `Z` within 400ms, a normal zip runs.
    pub zip_awaiting_second_press: Option<std::time::Instant>,
    pub pending_zip_transfer: bool,
    pub delete_confirm_yes: bool,
    pub pending_delete_operation: Option<PendingDeleteOperation>,
    pub extract_conflict_overwrite: bool,
    pub pending_extract_operation: Option<PendingExtractOperation>,
    pub preferences: UserPreferences,
    pub settings_selected_index: usize,
    pub settings_editing_editor: bool,
    pub last_terminal_cwd_sync: Option<Instant>,
    pub local_terminal: Option<LocalTerminal>,
    pub local_terminal_visible: bool,
    pub terminal_focus: TerminalFocus,
    keyboard_shortcuts_return_state: Option<KeyboardShortcutsReturnState>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalFocus {
    None,
    LocalTerminal,
    RemoteTerminal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct KeyboardShortcutsReturnState {
    mode: AppMode,
    focus: FocusPanel,
    active_menu_tab: MenuTab,
}

impl App {
    pub fn new() -> Result<Self> {
        let db = Database::new()?;
        let all_connections = db.get_all_connections()?;
        let recent_connections = db.get_recent_connections(10)?;
        let default_editor = detect_default_editor();
        let preferences = UserPreferences::load_from_db(&db, &default_editor)?;
        let local = LocalBrowser::new(preferences.explorer_sort)?;

        let parallel_transfers = std::thread::available_parallelism()
            .map(|parallelism| parallelism.get().clamp(4, 8))
            .unwrap_or(4);

        Ok(Self {
            running: true,
            mode: AppMode::Normal,
            focus: FocusPanel::Local,
            active_menu_tab: MenuTab::File,
            file_menu_index: 0,
            connect_menu_index: 0,
            help_menu_index: 0,
            shortcuts_scroll_offset: 0,
            shortcuts_viewport_height: 1,
            shortcuts_help_line_count: 47,
            connection_dialog: ConnectionDialog::new(),
            db,
            local,
            tabs: Vec::new(),
            active_tab: 0,
            tab_bar_highlight: 0,
            next_tab_id: 1,
            all_connections,
            recent_connections,
            connection_list_index: 0,
            showing_recent: false,
            status_message: None,
            error_message: None,
            visible_file_rows: 10,
            visible_local_terminal_rows: 1,
            visible_remote_terminal_rows: 1,
            transfer_manager: TransferManager::new(parallel_transfers),
            editor_manager: EditorManager::new()?,
            directory_input: String::new(),
            directory_completions: Vec::new(),
            directory_completion_index: 0,
            rename_input: String::new(),
            rename_old_name: String::new(),
            last_slash_press: None,
            zip_awaiting_second_press: None,
            pending_zip_transfer: false,
            delete_confirm_yes: true,
            pending_delete_operation: None,
            extract_conflict_overwrite: true,
            pending_extract_operation: None,
            preferences,
            settings_selected_index: 0,
            settings_editing_editor: false,
            last_terminal_cwd_sync: None,
            local_terminal: None,
            local_terminal_visible: false,
            terminal_focus: TerminalFocus::None,
            keyboard_shortcuts_return_state: None,
        })
    }

    pub fn quit(&mut self) {
        self.running = false;
    }

    #[allow(dead_code)]
    pub fn focus_menu(&mut self) {
        self.mode = AppMode::MenuFocused;
    }

    /// Clears Space / Shift+arrow multi-selection on the focused file panel. Returns `true` if a selection existed.
    pub fn try_clear_file_panel_selection(&mut self) -> bool {
        match self.focus {
            FocusPanel::Local => {
                if !self.local.browser.selected_indices.is_empty() {
                    self.local.browser.clear_selection();
                    true
                } else {
                    false
                }
            }
            FocusPanel::Remote => {
                if let Some(tab) = self.current_tab_mut() {
                    if !tab.browser.selected_indices.is_empty() {
                        tab.browser.clear_selection();
                        true
                    } else {
                        false
                    }
                } else {
                    false
                }
            }
            FocusPanel::ConnectionTabs => false,
        }
    }

    pub fn open_file_menu(&mut self) {
        self.active_menu_tab = MenuTab::File;
        self.file_menu_index = 0;
        self.mode = AppMode::MenuFocused;
    }

    pub fn open_dropdown(&mut self) {
        if self.mode == AppMode::MenuFocused {
            self.mode = AppMode::MenuOpen;
        }
    }

    pub fn close_menu(&mut self) {
        self.mode = if !self.tabs.is_empty() {
            AppMode::Connected
        } else {
            AppMode::Normal
        };
    }

    pub fn next_menu_tab(&mut self) {
        self.active_menu_tab = match self.active_menu_tab {
            MenuTab::File => MenuTab::Connect,
            MenuTab::Connect => MenuTab::Help,
            MenuTab::Help => MenuTab::File,
        };
    }

    pub fn prev_menu_tab(&mut self) {
        self.active_menu_tab = match self.active_menu_tab {
            MenuTab::File => MenuTab::Help,
            MenuTab::Connect => MenuTab::File,
            MenuTab::Help => MenuTab::Connect,
        };
    }

    pub fn menu_down(&mut self) {
        match self.active_menu_tab {
            MenuTab::File => {
                self.file_menu_index = (self.file_menu_index + 1) % 2;
            }
            MenuTab::Connect => {
                self.connect_menu_index = (self.connect_menu_index + 1) % 3;
            }
            MenuTab::Help => {
                self.help_menu_index = 0;
            }
        }
    }

    pub fn menu_up(&mut self) {
        match self.active_menu_tab {
            MenuTab::File => {
                self.file_menu_index = if self.file_menu_index == 0 { 1 } else { 0 };
            }
            MenuTab::Connect => {
                self.connect_menu_index = if self.connect_menu_index == 0 {
                    2
                } else {
                    self.connect_menu_index - 1
                };
            }
            MenuTab::Help => {
                self.help_menu_index = 0;
            }
        }
    }

    pub fn select_menu_item(&mut self) {
        match self.active_menu_tab {
            MenuTab::File => match self.file_menu_index {
                0 => self.open_settings(),
                1 => self.quit(),
                _ => {}
            },
            MenuTab::Connect => match self.connect_menu_index {
                0 => {
                    self.connection_dialog = ConnectionDialog::new();
                    self.mode = AppMode::ConnectionDialog;
                }
                1 => {
                    self.showing_recent = true;
                    self.connection_list_index = 0;
                    self.refresh_connections();
                    self.mode = AppMode::ConnectionList;
                }
                2 => {
                    self.showing_recent = false;
                    self.connection_list_index = 0;
                    self.refresh_connections();
                    self.mode = AppMode::ConnectionList;
                }
                _ => {}
            },
            MenuTab::Help => {
                self.open_keyboard_shortcuts();
            }
        }
    }

    pub fn can_open_keyboard_shortcuts(&self) -> bool {
        if self.terminal_focus != TerminalFocus::None {
            return false;
        }

        match self.mode {
            AppMode::Normal => {
                self.focus == FocusPanel::Local && !self.local.browser.is_filtering()
            }
            AppMode::Connected => match self.focus {
                FocusPanel::Local => !self.local.browser.is_filtering(),
                FocusPanel::Remote => self
                    .current_tab()
                    .map(|tab| !tab.browser.is_filtering())
                    .unwrap_or(false),
                FocusPanel::ConnectionTabs => false,
            },
            AppMode::MenuFocused => true,
            _ => false,
        }
    }

    pub fn open_keyboard_shortcuts(&mut self) {
        self.shortcuts_scroll_offset = 0;
        self.keyboard_shortcuts_return_state = Some(KeyboardShortcutsReturnState {
            mode: self.mode,
            focus: self.focus,
            active_menu_tab: self.active_menu_tab,
        });
        self.mode = AppMode::KeyboardShortcuts;
    }

    pub fn close_keyboard_shortcuts(&mut self) {
        self.shortcuts_scroll_offset = 0;
        if let Some(state) = self.keyboard_shortcuts_return_state.take() {
            self.mode = state.mode;
            self.focus = state.focus;
            self.active_menu_tab = state.active_menu_tab;
        } else {
            self.mode = if !self.tabs.is_empty() {
                AppMode::Connected
            } else {
                AppMode::Normal
            };
        }
    }

    pub fn shortcuts_scroll_up(&mut self) {
        self.shortcuts_scroll_offset = self.shortcuts_scroll_offset.saturating_sub(1);
    }

    pub fn shortcuts_scroll_down(&mut self, max_items: usize, visible_height: usize) {
        let vh = visible_height.max(1);
        if max_items > vh {
            let max_scroll = max_items - vh;
            if self.shortcuts_scroll_offset < max_scroll {
                self.shortcuts_scroll_offset += 1;
            }
        }
    }

    /// Keeps scroll offset valid when the terminal is resized or the shortcuts dialog is first shown.
    pub fn clamp_shortcuts_scroll(&mut self) {
        let n = self.shortcuts_help_line_count.max(1);
        let vh = self.shortcuts_viewport_height.max(1);
        if n > vh {
            let max_scroll = n - vh;
            self.shortcuts_scroll_offset = self.shortcuts_scroll_offset.min(max_scroll);
        } else {
            self.shortcuts_scroll_offset = 0;
        }
    }

    /// After a single `Z`, wait 400ms for a second `Z`; otherwise run a normal zip once.
    pub fn flush_zip_single_press(&mut self) {
        let Some(t) = self.zip_awaiting_second_press else {
            return;
        };
        if Instant::now().duration_since(t) >= Duration::from_millis(400) {
            self.zip_awaiting_second_press = None;
            self.pending_zip_transfer = false;
            self.zip_selected();
        }
    }

    fn effective_local_terminal_dir(&self) -> String {
        if self.preferences.open_terminal_in_explorer_dir {
            self.local.browser.current_dir.clone()
        } else {
            dirs::home_dir()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_else(|| "/".to_string())
        }
    }

    fn effective_remote_terminal_dir(&self) -> String {
        if !self.preferences.open_terminal_in_explorer_dir {
            return "/".to_string();
        }
        self.current_tab()
            .map(|t| t.browser.current_dir.clone())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "/".to_string())
    }

    pub fn open_settings(&mut self) {
        self.settings_selected_index = 0;
        self.settings_editing_editor = false;
        self.mode = AppMode::Settings;
    }

    pub fn close_settings(&mut self) {
        self.settings_editing_editor = false;
        self.mode = if !self.tabs.is_empty() {
            AppMode::Connected
        } else {
            AppMode::Normal
        };
    }

    pub fn persist_preferences(&mut self) {
        if let Err(e) = self.preferences.save_to_db(&self.db) {
            error!("Failed to save settings: {}", e);
        }
    }

    fn apply_explorer_sort_preferences(&mut self) {
        let sort = self.preferences.explorer_sort;
        self.local.browser.sort = sort;
        self.local.browser.sort_files();
        for tab in &mut self.tabs {
            tab.browser.sort = sort;
            tab.browser.sort_files();
        }
    }

    pub fn settings_move_up(&mut self) {
        if self.settings_editing_editor {
            return;
        }
        if self.settings_selected_index > 0 {
            self.settings_selected_index -= 1;
        }
    }

    pub fn settings_move_down(&mut self) {
        if self.settings_editing_editor {
            return;
        }
        if self.settings_selected_index + 1 < SETTINGS_ROW_COUNT {
            self.settings_selected_index += 1;
        }
    }

    pub fn settings_adjust_row(&mut self, forward: bool) {
        if self.settings_editing_editor {
            return;
        }
        match self.settings_selected_index {
            0 => {
                self.preferences.explorer_columns.show_headers =
                    !self.preferences.explorer_columns.show_headers
            }
            1 => {
                self.preferences.explorer_columns.show_size =
                    !self.preferences.explorer_columns.show_size
            }
            2 => {
                self.preferences.explorer_columns.show_permissions =
                    !self.preferences.explorer_columns.show_permissions
            }
            3 => {
                self.preferences.explorer_columns.show_modified =
                    !self.preferences.explorer_columns.show_modified
            }
            4 => {
                self.preferences.explorer_columns.show_created =
                    !self.preferences.explorer_columns.show_created
            }
            5 => {
                self.preferences.explorer_sort.field = if forward {
                    self.preferences.explorer_sort.field.next()
                } else {
                    self.preferences.explorer_sort.field.prev()
                };
            }
            6 => {
                self.preferences.explorer_sort.direction = if forward {
                    self.preferences.explorer_sort.direction.next()
                } else {
                    self.preferences.explorer_sort.direction.prev()
                };
            }
            7 => {
                return;
            }
            8 => {
                self.preferences.open_terminal_in_explorer_dir =
                    !self.preferences.open_terminal_in_explorer_dir
            }
            9 => {
                self.preferences.explorer_follows_terminal =
                    !self.preferences.explorer_follows_terminal
            }
            _ => return,
        }
        if matches!(self.settings_selected_index, 5 | 6) {
            self.apply_explorer_sort_preferences();
        }
        self.persist_preferences();
    }

    pub fn settings_begin_editor_edit(&mut self) {
        if self.settings_selected_index == SETTINGS_ROW_EDITOR {
            self.settings_editing_editor = true;
        }
    }

    pub fn settings_finish_editor_edit(&mut self) {
        self.settings_editing_editor = false;
        self.persist_preferences();
    }

    pub fn settings_editor_add_char(&mut self, c: char) {
        self.preferences.editor_command.push(c);
    }

    pub fn settings_editor_remove_char(&mut self) {
        self.preferences.editor_command.pop();
    }

    /// Throttled sync of file explorer cwd from the visible PTY when `explorer_follows_terminal` is on:
    /// local panel uses the local shell PID cwd; remote uses OSC 7 emitted by bash/zsh hooks in `RemoteTerminal`.
    pub fn maybe_sync_explorers_from_terminals(&mut self) {
        if !self.preferences.explorer_follows_terminal {
            return;
        }
        let now = Instant::now();
        if let Some(last) = self.last_terminal_cwd_sync {
            if now.duration_since(last) < Duration::from_millis(400) {
                return;
            }
        }
        self.last_terminal_cwd_sync = Some(now);

        if self.local_terminal_visible {
            if let Some(term) = self.local_terminal.as_ref() {
                if let Some(cwd) = term.try_shell_cwd() {
                    if cwd.is_dir() {
                        let cwd_str = cwd.to_string_lossy().to_string();
                        if cwd_str != self.local.browser.current_dir {
                            if LocalBrowser::change_directory(&mut self.local.browser, &cwd_str)
                                .is_ok()
                            {
                                self.update_local_watcher();
                            }
                        }
                    }
                }
            }
        }

        let active_tab = self.active_tab;
        if let Some(tab) = self.tabs.get_mut(active_tab) {
            if tab.remote_terminal_visible {
                if let Some(cwd) = tab
                    .remote_terminal
                    .as_ref()
                    .and_then(|t| t.try_reported_cwd())
                {
                    if cwd != tab.browser.current_dir {
                        let _ = tab.change_directory(&cwd);
                    }
                }
            }
        }
    }

    pub fn toggle_local_terminal(&mut self) {
        self.local_terminal_visible = !self.local_terminal_visible;

        if self.local_terminal_visible {
            let current_dir = self.effective_local_terminal_dir();
            if let Some(term) = self.local_terminal.as_mut() {
                if let Err(e) = term.set_working_dir(&current_dir) {
                    error!("Failed to sync local terminal directory: {}", e);
                    self.error_message = Some(format!("Failed to sync terminal directory: {}", e));
                    self.local_terminal_visible = false;
                }
            } else {
                match LocalTerminal::new(&current_dir) {
                    Ok(term) => {
                        info!("Local terminal created");
                        self.local_terminal = Some(term);
                    }
                    Err(e) => {
                        error!("Failed to create local terminal: {}", e);
                        self.error_message = Some(format!("Failed to create terminal: {}", e));
                        self.local_terminal_visible = false;
                    }
                }
            }
        }

        if self.local_terminal_visible && self.focus == FocusPanel::Local {
            self.terminal_focus = TerminalFocus::LocalTerminal;
        } else if !self.local_terminal_visible
            && self.terminal_focus == TerminalFocus::LocalTerminal
        {
            self.terminal_focus = TerminalFocus::None;
        }
    }

    pub fn toggle_remote_terminal(&mut self) {
        let active_tab = self.active_tab;
        if let Some(tab) = self.tabs.get_mut(active_tab) {
            tab.remote_terminal_visible = !tab.remote_terminal_visible;
        }

        let visible = self
            .tabs
            .get(active_tab)
            .map(|tab| tab.remote_terminal_visible)
            .unwrap_or(false);

        if visible {
            let (session_info, tab_name) = match self.tabs.get(active_tab) {
                Some(tab) => (tab.session_info.clone(), tab.name.clone()),
                None => return,
            };
            let current_dir = self.effective_remote_terminal_dir();

            let result = if let Some(tab) = self.tabs.get_mut(active_tab) {
                if let Some(term) = tab.remote_terminal.as_mut() {
                    term.set_working_dir(&current_dir)
                } else {
                    match RemoteTerminal::new(&session_info, &current_dir) {
                        Ok(term) => {
                            info!("Remote terminal created for {}", tab_name);
                            tab.remote_terminal = Some(term);
                            Ok(())
                        }
                        Err(e) => Err(e),
                    }
                }
            } else {
                return;
            };

            if let Err(e) = result {
                error!("Failed to open remote terminal: {}", e);
                self.error_message = Some(format!("Failed to create terminal: {}", e));
                if let Some(tab) = self.tabs.get_mut(active_tab) {
                    tab.remote_terminal_visible = false;
                }
            }
        }

        let visible = self
            .tabs
            .get(active_tab)
            .map(|tab| tab.remote_terminal_visible)
            .unwrap_or(false);

        if visible && self.focus == FocusPanel::Remote {
            self.terminal_focus = TerminalFocus::RemoteTerminal;
        } else if !visible && self.terminal_focus == TerminalFocus::RemoteTerminal {
            self.terminal_focus = TerminalFocus::None;
        }
    }

    #[allow(dead_code)]
    pub fn is_local_terminal_visible(&self) -> bool {
        self.local_terminal_visible
    }

    pub fn is_remote_terminal_visible(&self) -> bool {
        self.tabs
            .get(self.active_tab)
            .map(|t| t.remote_terminal_visible)
            .unwrap_or(false)
    }

    pub fn poll_remote_terminals(&mut self) {
        for tab in &mut self.tabs {
            if let Some(term) = &mut tab.remote_terminal {
                match term.poll_read() {
                    Ok(Some(message)) => {
                        self.status_message = Some(message);
                        self.error_message = None;
                    }
                    Ok(None) => {}
                    Err(e) => {
                        self.status_message = None;
                        self.error_message = Some(format!(
                            "Remote terminal reconnect failed for {}: {}",
                            tab.name, e
                        ));
                    }
                }
            }
        }
    }

    pub fn refresh_connections(&mut self) {
        if let Ok(all) = self.db.get_all_connections() {
            self.all_connections = all;
        }
        if let Ok(recent) = self.db.get_recent_connections(10) {
            self.recent_connections = recent;
        }
    }

    pub fn close_dialog(&mut self) {
        self.mode = if !self.tabs.is_empty() {
            AppMode::Connected
        } else {
            AppMode::Normal
        };
        self.connection_dialog.error_message = None;
    }

    pub fn dialog_next_field(&mut self) {
        self.connection_dialog.active_field = self.connection_dialog.active_field.next();
    }

    pub fn dialog_prev_field(&mut self) {
        self.connection_dialog.active_field = self.connection_dialog.active_field.prev();
    }

    pub fn dialog_input(&mut self, c: char) {
        let field = self
            .connection_dialog
            .get_field_value_mut(self.connection_dialog.active_field);
        field.push(c);
    }

    pub fn dialog_backspace(&mut self) {
        let field = self
            .connection_dialog
            .get_field_value_mut(self.connection_dialog.active_field);
        field.pop();
    }

    pub fn try_connect(&mut self) -> Result<()> {
        let params = self.connection_dialog.to_connection_params()?;

        let conn = SshConnection::connect(&params)?;

        let auth_method = if params.password.is_some() {
            AuthMethod::Password
        } else if let Some(ref path) = params.key_path {
            AuthMethod::KeyFile(path.clone())
        } else {
            AuthMethod::DefaultKey
        };

        let name = if self.connection_dialog.name.is_empty() {
            format!("{}@{}", params.username, params.host)
        } else {
            self.connection_dialog.name.clone()
        };

        let saved = SavedConnection::new(
            name.clone(),
            params.host.clone(),
            params.port,
            params.username.clone(),
            auth_method,
        );

        let _ = self.db.save_connection(&saved);
        let _ = self.db.update_last_used(&saved.id);

        let tab_id = self.next_tab_id;
        self.next_tab_id += 1;

        let session_info = SftpSessionInfo {
            host: params.host.clone(),
            port: params.port,
            username: params.username.clone(),
            password: params.password.clone(),
            key_path: params.key_path.clone(),
        };

        let mut tab = ConnectionTab::new(
            tab_id,
            name.clone(),
            conn,
            session_info,
            self.preferences.explorer_sort,
        );
        tab.refresh_directory_reset_index()?;

        self.tabs.push(tab);
        self.active_tab = self.tabs.len() - 1;
        self.mode = AppMode::Connected;
        self.focus = FocusPanel::Remote;
        self.refresh_connections();
        self.status_message = Some(format!("Connected to {}", params.host));
        self.error_message = None;

        Ok(())
    }

    pub fn connect_to_saved(&mut self, saved: &SavedConnection) -> Result<()> {
        let params = ConnectionParams {
            host: saved.host.clone(),
            port: saved.port,
            username: saved.username.clone(),
            password: None,
            key_path: match &saved.auth_method {
                AuthMethod::KeyFile(path) => Some(path.clone()),
                _ => None,
            },
        };

        let conn = SshConnection::connect(&params)?;

        let _ = self.db.update_last_used(&saved.id);

        let tab_id = self.next_tab_id;
        self.next_tab_id += 1;

        let session_info = SftpSessionInfo {
            host: params.host.clone(),
            port: params.port,
            username: params.username.clone(),
            password: params.password.clone(),
            key_path: params.key_path.clone(),
        };

        let mut tab = ConnectionTab::new(
            tab_id,
            saved.name.clone(),
            conn,
            session_info,
            self.preferences.explorer_sort,
        );
        tab.refresh_directory_reset_index()?;

        self.tabs.push(tab);
        self.active_tab = self.tabs.len() - 1;
        self.mode = AppMode::Connected;
        self.focus = FocusPanel::Remote;
        self.refresh_connections();
        self.status_message = Some(format!("Connected to {}", params.host));
        self.error_message = None;

        Ok(())
    }

    #[allow(dead_code)]
    pub fn disconnect_current(&mut self) {
        if self.tabs.is_empty() {
            return;
        }

        let tab = self.tabs.remove(self.active_tab);
        tab.connection.disconnect();

        if self.tabs.is_empty() {
            self.active_tab = 0;
            self.mode = AppMode::Normal;
            self.focus = FocusPanel::Local;
            self.status_message = Some("Disconnected".to_string());
        } else {
            self.active_tab = self.active_tab.min(self.tabs.len() - 1);
            self.tab_bar_highlight = self.tab_bar_highlight.min(self.tabs.len() - 1);
            if self.tabs.len() <= 1 && self.focus == FocusPanel::ConnectionTabs {
                self.focus = FocusPanel::Local;
            }
        }
    }

    pub fn current_tab(&self) -> Option<&ConnectionTab> {
        self.tabs.get(self.active_tab)
    }

    pub fn current_tab_mut(&mut self) -> Option<&mut ConnectionTab> {
        self.tabs.get_mut(self.active_tab)
    }

    #[allow(dead_code)]
    pub fn next_connection_tab(&mut self) {
        if !self.tabs.is_empty() {
            self.active_tab = (self.active_tab + 1) % self.tabs.len();
        }
    }

    #[allow(dead_code)]
    pub fn prev_connection_tab(&mut self) {
        if !self.tabs.is_empty() {
            self.active_tab = if self.active_tab == 0 {
                self.tabs.len() - 1
            } else {
                self.active_tab - 1
            };
        }
    }

    /// Tab order: menu bar → local → remote → [connection tabs when 2+ sessions] → menu bar.
    pub fn cycle_main_focus_forward(&mut self) {
        match self.mode {
            AppMode::MenuFocused => {
                self.close_menu();
                self.focus = FocusPanel::Local;
            }
            AppMode::Normal => match self.focus {
                FocusPanel::Local => self.focus = FocusPanel::Remote,
                FocusPanel::Remote => self.open_file_menu(),
                FocusPanel::ConnectionTabs => self.focus = FocusPanel::Local,
            },
            AppMode::Connected => match self.focus {
                FocusPanel::Local => self.focus = FocusPanel::Remote,
                FocusPanel::Remote => {
                    if self.tabs.len() > 1 {
                        self.focus = FocusPanel::ConnectionTabs;
                        self.tab_bar_highlight = self.active_tab.min(self.tabs.len() - 1);
                    } else {
                        self.open_file_menu();
                    }
                }
                FocusPanel::ConnectionTabs => {
                    self.open_file_menu();
                }
            },
            _ => {}
        }
    }

    /// Reverse of [`Self::cycle_main_focus_forward`].
    pub fn cycle_main_focus_backward(&mut self) {
        match self.mode {
            AppMode::MenuFocused => {
                self.close_menu();
                if self.tabs.len() > 1 {
                    self.focus = FocusPanel::ConnectionTabs;
                    self.tab_bar_highlight = self.active_tab.min(self.tabs.len() - 1);
                } else {
                    self.focus = FocusPanel::Remote;
                }
            }
            AppMode::Normal => match self.focus {
                FocusPanel::Local => self.open_file_menu(),
                FocusPanel::Remote => self.focus = FocusPanel::Local,
                FocusPanel::ConnectionTabs => self.focus = FocusPanel::Local,
            },
            AppMode::Connected => match self.focus {
                FocusPanel::Local => self.open_file_menu(),
                FocusPanel::Remote => self.focus = FocusPanel::Local,
                FocusPanel::ConnectionTabs => self.focus = FocusPanel::Remote,
            },
            _ => {}
        }
    }

    pub fn tab_bar_highlight_next(&mut self) {
        if self.tabs.len() <= 1 {
            return;
        }
        self.tab_bar_highlight = (self.tab_bar_highlight + 1) % self.tabs.len();
    }

    pub fn tab_bar_highlight_prev(&mut self) {
        if self.tabs.len() <= 1 {
            return;
        }
        self.tab_bar_highlight = if self.tab_bar_highlight == 0 {
            self.tabs.len() - 1
        } else {
            self.tab_bar_highlight - 1
        };
    }

    /// Applies `tab_bar_highlight` as the active connection and returns focus to the local panel.
    pub fn activate_highlighted_connection_tab(&mut self) {
        if self.tabs.is_empty() {
            return;
        }
        self.tab_bar_highlight = self.tab_bar_highlight.min(self.tabs.len() - 1);
        self.active_tab = self.tab_bar_highlight;
        self.focus = FocusPanel::Local;
    }

    pub fn get_current_connections(&self) -> &[SavedConnection] {
        if self.showing_recent {
            &self.recent_connections
        } else {
            &self.all_connections
        }
    }

    pub fn connection_list_up(&mut self) {
        let len = self.get_current_connections().len();
        if len > 0 {
            self.connection_list_index = if self.connection_list_index == 0 {
                len - 1
            } else {
                self.connection_list_index - 1
            };
        }
    }

    pub fn connection_list_down(&mut self) {
        let len = self.get_current_connections().len();
        if len > 0 {
            self.connection_list_index = (self.connection_list_index + 1) % len;
        }
    }

    pub fn select_connection(&mut self) -> Result<()> {
        let connections = if self.showing_recent {
            &self.recent_connections
        } else {
            &self.all_connections
        };

        if let Some(saved) = connections.get(self.connection_list_index).cloned() {
            self.connect_to_saved(&saved)?;
        }

        Ok(())
    }

    pub fn handle_slash_press(&mut self) {
        let now = std::time::Instant::now();
        let is_double_tap = if let Some(last) = self.last_slash_press {
            now.duration_since(last).as_millis() < 400
        } else {
            false
        };

        self.last_slash_press = Some(now);

        if self.mode == AppMode::DirectoryInput && is_double_tap {
            self.directory_input = "/".to_string();
            self.directory_completions.clear();
            self.directory_completion_index = 0;
        } else if self.mode == AppMode::DirectoryInput {
            self.directory_input.push('/');
            self.directory_completions.clear();
            self.directory_completion_index = 0;
        } else {
            self.open_directory_input();
        }
    }

    pub fn open_directory_input(&mut self) {
        let current_dir = match self.focus {
            FocusPanel::Local | FocusPanel::ConnectionTabs => {
                self.local.browser.current_dir.clone()
            }
            FocusPanel::Remote => {
                if let Some(tab) = self.current_tab() {
                    tab.browser.current_dir.clone()
                } else {
                    String::new()
                }
            }
        };
        self.directory_input = if current_dir.ends_with('/') {
            current_dir
        } else {
            format!("{}/", current_dir)
        };
        self.directory_completions.clear();
        self.directory_completion_index = 0;
        self.mode = AppMode::DirectoryInput;
    }

    pub fn close_directory_input(&mut self) {
        self.directory_input.clear();
        self.directory_completions.clear();
        self.mode = if !self.tabs.is_empty() {
            AppMode::Connected
        } else {
            AppMode::Normal
        };
    }

    pub fn refresh_focused_explorer(&mut self) {
        match self.focus {
            FocusPanel::Local | FocusPanel::ConnectionTabs => {
                match LocalBrowser::refresh_files(&mut self.local.browser) {
                    Ok(()) => {
                        self.error_message = None;
                    }
                    Err(e) => {
                        self.error_message = Some(format!("Refresh failed: {}", e));
                    }
                }
            }
            FocusPanel::Remote => {
                if let Some(tab) = self.current_tab_mut() {
                    match tab.refresh_directory() {
                        Ok(()) => {
                            self.error_message = None;
                        }
                        Err(e) => {
                            self.error_message = Some(format!("Refresh failed: {}", e));
                        }
                    }
                }
            }
        }
    }

    pub fn try_begin_rename(&mut self) {
        if matches!(self.focus, FocusPanel::ConnectionTabs) {
            return;
        }
        let multi = match self.focus {
            FocusPanel::Local => self.local.browser.selected_indices.len() > 1,
            FocusPanel::Remote => self
                .current_tab()
                .map(|t| t.browser.selected_indices.len() > 1)
                .unwrap_or(false),
            FocusPanel::ConnectionTabs => false,
        };
        if multi {
            self.status_message = None;
            self.error_message =
                Some("Rename only works when a single item is selected".to_string());
            return;
        }
        let entry = match self.focus {
            FocusPanel::Local => self.local.browser.rename_target_entry().cloned(),
            FocusPanel::Remote => self
                .current_tab()
                .and_then(|t| t.browser.rename_target_entry().cloned()),
            FocusPanel::ConnectionTabs => None,
        };
        let Some(entry) = entry else {
            return;
        };
        if entry.name == ".." {
            self.error_message = Some("Cannot rename this entry".to_string());
            self.status_message = None;
            return;
        }
        self.rename_old_name = entry.name.clone();
        self.rename_input = entry.name.clone();
        self.error_message = None;
        self.mode = AppMode::RenameInput;
    }

    pub fn close_rename(&mut self) {
        self.rename_input.clear();
        self.rename_old_name.clear();
        self.mode = if !self.tabs.is_empty() {
            AppMode::Connected
        } else {
            AppMode::Normal
        };
    }

    pub fn rename_input_add_char(&mut self, c: char) {
        self.rename_input.push(c);
    }

    pub fn rename_input_remove_char(&mut self) {
        self.rename_input.pop();
    }

    pub fn commit_rename(&mut self) {
        let new_name = self.rename_input.trim().to_string();
        if new_name.is_empty() {
            self.error_message = Some("Name cannot be empty".to_string());
            self.status_message = None;
            return;
        }
        if new_name.contains('/') || new_name.contains('\\') {
            self.error_message = Some("Name cannot contain path separators".to_string());
            self.status_message = None;
            return;
        }
        if new_name == self.rename_old_name {
            self.close_rename();
            return;
        }

        let result = match self.focus {
            FocusPanel::Local => {
                let dir = PathBuf::from(&self.local.browser.current_dir);
                let from = dir.join(&self.rename_old_name);
                let to = dir.join(&new_name);
                std::fs::rename(&from, &to).map_err(|e| e.to_string())
            }
            FocusPanel::Remote => {
                if let Some(tab) = self.current_tab() {
                    let base = tab.browser.current_dir.trim_end_matches('/');
                    let old_path = format!("{}/{}", base, self.rename_old_name);
                    let new_path = format!("{}/{}", base, new_name);
                    match tab.connection.sftp() {
                        Ok(sftp) => sftp
                            .rename(Path::new(&old_path), Path::new(&new_path), None)
                            .map_err(|e| e.to_string()),
                        Err(e) => Err(e.to_string()),
                    }
                } else {
                    Err("No remote session".to_string())
                }
            }
            FocusPanel::ConnectionTabs => Err("Rename invalid for this focus".to_string()),
        };

        match result {
            Ok(()) => {
                self.error_message = None;
                self.status_message = Some(format!("Renamed to \"{}\"", new_name));
                match self.focus {
                    FocusPanel::Local => {
                        let _ = LocalBrowser::refresh_files(&mut self.local.browser);
                        self.update_local_watcher();
                    }
                    FocusPanel::Remote => {
                        if let Some(tab) = self.current_tab_mut() {
                            let _ = tab.refresh_directory();
                        }
                    }
                    FocusPanel::ConnectionTabs => {}
                }
                self.close_rename();
            }
            Err(e) => {
                self.status_message = None;
                self.error_message = Some(format!("Rename failed: {}", e));
            }
        }
    }

    pub fn directory_input_add_char(&mut self, c: char) {
        self.directory_input.push(c);
        self.directory_completions.clear();
        self.directory_completion_index = 0;
    }

    pub fn directory_input_remove_char(&mut self) {
        self.directory_input.pop();
        self.directory_completions.clear();
        self.directory_completion_index = 0;
    }

    pub fn directory_input_tab_complete(&mut self) {
        if self.directory_completions.is_empty() {
            self.directory_completions = self.get_directory_completions();
            self.directory_completion_index = 0;

            if self.directory_completions.len() == 1 {
                self.directory_input = self.directory_completions[0].clone();
                if !self.directory_input.ends_with('/') {
                    self.directory_input.push('/');
                }
                self.directory_completions.clear();
            } else if !self.directory_completions.is_empty() {
                self.directory_input = self.directory_completions[0].clone();
            }
        } else {
            self.directory_completion_index =
                (self.directory_completion_index + 1) % self.directory_completions.len();
            self.directory_input =
                self.directory_completions[self.directory_completion_index].clone();
        }
    }

    fn get_directory_completions(&self) -> Vec<String> {
        let input = &self.directory_input;

        let (dir_path, partial) = if let Some(pos) = input.rfind('/') {
            let dir = &input[..=pos];
            let partial = &input[pos + 1..];
            (dir.to_string(), partial.to_string())
        } else {
            let current = match self.focus {
                FocusPanel::Local | FocusPanel::ConnectionTabs => &self.local.browser.current_dir,
                FocusPanel::Remote => {
                    if let Some(tab) = self.current_tab() {
                        &tab.browser.current_dir
                    } else {
                        return Vec::new();
                    }
                }
            };
            (format!("{}/", current), input.clone())
        };

        let partial_lower = partial.to_lowercase();

        match self.focus {
            FocusPanel::Local | FocusPanel::ConnectionTabs => {
                let expanded_dir = if dir_path.starts_with('~') {
                    if let Some(home) = dirs::home_dir() {
                        dir_path.replacen('~', &home.to_string_lossy(), 1)
                    } else {
                        dir_path.clone()
                    }
                } else {
                    dir_path.clone()
                };

                if let Ok(entries) = std::fs::read_dir(&expanded_dir) {
                    entries
                        .filter_map(|e| e.ok())
                        .filter(|e| {
                            if let Ok(meta) = e.metadata() {
                                if !meta.is_dir() {
                                    return false;
                                }
                            }
                            let name = e.file_name().to_string_lossy().to_string();
                            if partial.is_empty() {
                                !name.starts_with('.')
                            } else if partial.starts_with('.') {
                                name.to_lowercase().starts_with(&partial_lower)
                            } else {
                                !name.starts_with('.')
                                    && name.to_lowercase().starts_with(&partial_lower)
                            }
                        })
                        .map(|e| {
                            let name = e.file_name().to_string_lossy().to_string();
                            format!("{}{}", dir_path, name)
                        })
                        .collect()
                } else {
                    Vec::new()
                }
            }
            FocusPanel::Remote => {
                if let Some(tab) = self.current_tab() {
                    let check_dir = if dir_path.is_empty() {
                        "/"
                    } else {
                        dir_path.trim_end_matches('/')
                    };

                    let cmd = format!("ls -1pa {} 2>/dev/null | grep '/$'", check_dir);
                    if let Ok(output) = tab.connection.exec(&cmd) {
                        output
                            .lines()
                            .filter_map(|line| {
                                let name = line.trim_end_matches('/');
                                if name.is_empty() || name == "." || name == ".." {
                                    return None;
                                }
                                if partial.is_empty() {
                                    if name.starts_with('.') {
                                        None
                                    } else {
                                        Some(format!("{}{}", dir_path, name))
                                    }
                                } else if partial.starts_with('.') {
                                    if name.to_lowercase().starts_with(&partial_lower) {
                                        Some(format!("{}{}", dir_path, name))
                                    } else {
                                        None
                                    }
                                } else {
                                    // Normal completion - skip hidden files
                                    if !name.starts_with('.')
                                        && name.to_lowercase().starts_with(&partial_lower)
                                    {
                                        Some(format!("{}{}", dir_path, name))
                                    } else {
                                        None
                                    }
                                }
                            })
                            .collect()
                    } else {
                        Vec::new()
                    }
                } else {
                    Vec::new()
                }
            }
        }
    }

    pub fn navigate_to_directory(&mut self) {
        let path = self.directory_input.trim().to_string();
        if path.is_empty() {
            self.close_directory_input();
            return;
        }

        info!("Navigating to directory: {}", path);
        self.error_message = None;
        self.status_message = None;

        let result = match self.focus {
            FocusPanel::Local | FocusPanel::ConnectionTabs => {
                let res = LocalBrowser::change_directory(&mut self.local.browser, &path);
                if res.is_ok() {
                    self.update_local_watcher();
                }
                res
            }
            FocusPanel::Remote => {
                if let Some(tab) = self.current_tab_mut() {
                    tab.change_directory(&path)
                } else {
                    Ok(())
                }
            }
        };

        if let Err(e) = result {
            self.error_message = Some(format!("Cannot navigate: {}", e));
        }

        self.close_directory_input();
    }

    pub fn check_transfers_and_refresh(&mut self) {
        let items = self.transfer_manager.get_items();
        let mut should_refresh_local = false;
        let mut should_refresh_remote = false;
        let mut had_completed = false;

        for item in items.iter() {
            if matches!(item.status, TransferStatus::Completed) {
                had_completed = true;
                let local_path = if item.is_download {
                    &item.dest_path
                } else {
                    &item.source_path
                };

                if local_path.starts_with(&self.local.browser.current_dir) {
                    should_refresh_local = true;
                }

                if let Some(tab) = self.current_tab() {
                    let remote_path = if item.is_download {
                        &item.source_path
                    } else {
                        &item.dest_path
                    };
                    if remote_path.starts_with(&tab.browser.current_dir) {
                        should_refresh_remote = true;
                    }
                }
            }
        }

        if should_refresh_local {
            let _ = LocalBrowser::refresh_files(&mut self.local.browser);
        }

        if should_refresh_remote {
            if let Some(tab) = self.current_tab_mut() {
                let _ = tab.refresh_directory();
            }
        }

        if had_completed {
            self.status_message = None;
        }

        self.transfer_manager.clear_completed();
    }

    pub fn update_local_watcher(&mut self) {
        let dir = PathBuf::from(&self.local.browser.current_dir);
        if let Err(e) = self.editor_manager.watch_local_directory(&dir) {
            debug!("Failed to watch local directory: {}", e);
        }
    }

    pub fn check_local_directory_changes(&mut self) {
        if self.editor_manager.check_local_changed() {
            info!("Local directory changed, refreshing file list");
            let _ = LocalBrowser::refresh_files(&mut self.local.browser);
        }
    }

    pub fn process_editor_uploads(&mut self) {
        let pending = self.editor_manager.take_pending_uploads();
        for upload in pending {
            info!(
                "Queueing editor sync upload: {:?} -> {}",
                upload.local_path, upload.remote_path
            );
            self.transfer_manager.queue_upload_to_path(
                upload.session_info,
                upload.local_path.to_string_lossy().to_string(),
                upload.remote_path,
                false,
                None,
            );
        }
    }

    pub fn download_selected(&mut self) {
        if self.focus != FocusPanel::Remote {
            return;
        }

        let Some(tab) = self.current_tab() else {
            return;
        };

        let files: Vec<_> = tab
            .browser
            .get_selected_files()
            .iter()
            .filter(|f| f.name != "..")
            .map(|f| (f.name.clone(), f.is_dir, f.size_bytes))
            .collect();
        let session_info = tab.session_info.clone();
        let remote_dir = tab.browser.current_dir.clone();
        let sftp = match tab.connection.sftp() {
            Ok(sftp) => sftp,
            Err(e) => {
                self.error_message = Some(format!("Cannot start download: {}", e));
                return;
            }
        };

        let local_dir = self.local.browser.current_dir.clone();
        let mut plans = Vec::new();

        for (name, is_dir, size_bytes) in &files {
            let remote_path = format!("{}/{}", remote_dir.trim_end_matches('/'), name);
            let dest_path = PathBuf::from(&local_dir).join(name);
            info!("  Download: {} (is_dir: {})", remote_path, is_dir);
            if let Err(e) = Self::collect_remote_downloads(
                &sftp,
                Path::new(&remote_path),
                &dest_path,
                *is_dir,
                *size_bytes,
                &mut plans,
            ) {
                self.error_message = Some(format!("Cannot queue download: {}", e));
                return;
            }
        }

        let count = plans.len();
        if count == 0 {
            self.status_message = Some("Created empty local directories".to_string());
            self.error_message = None;
            if let Some(tab) = self.current_tab_mut() {
                tab.browser.clear_selection();
            }
            return;
        }

        info!(
            "Queuing {} file(s) for download from {} to {}",
            count, remote_dir, local_dir
        );

        for plan in plans {
            self.transfer_manager.queue_download_to_path(
                session_info.clone(),
                plan.remote_path,
                plan.dest_path,
                false,
                Some(plan.total_bytes),
            );
        }

        self.status_message = Some(format!("Queued {} file(s) for download", count));
        self.error_message = None;

        if let Some(tab) = self.current_tab_mut() {
            tab.browser.clear_selection();
        }
    }

    fn collect_remote_downloads(
        sftp: &Sftp,
        remote_path: &Path,
        local_path: &Path,
        is_dir: bool,
        known_size: Option<u64>,
        plans: &mut Vec<DownloadPlanItem>,
    ) -> Result<()> {
        if is_dir {
            fs::create_dir_all(local_path)?;
            let entries = sftp.readdir(remote_path)?;

            for (path, stat) in entries {
                let filename = path
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string();
                if filename == "." || filename == ".." {
                    continue;
                }

                let child_remote = path;
                let child_local = local_path.join(&filename);
                Self::collect_remote_downloads(
                    sftp,
                    &child_remote,
                    &child_local,
                    stat.is_dir(),
                    stat.size,
                    plans,
                )?;
            }

            return Ok(());
        }

        if let Some(parent) = local_path.parent() {
            fs::create_dir_all(parent)?;
        }

        let total_bytes = match known_size {
            Some(size) => size,
            None => sftp
                .stat(remote_path)
                .ok()
                .and_then(|stat| stat.size)
                .unwrap_or(0),
        };

        plans.push(DownloadPlanItem {
            remote_path: remote_path.to_string_lossy().to_string(),
            dest_path: local_path.to_string_lossy().to_string(),
            total_bytes,
        });

        Ok(())
    }

    pub fn upload_selected(&mut self) {
        if self.focus != FocusPanel::Local {
            return;
        }

        if self.tabs.is_empty() {
            return;
        }

        let files: Vec<_> = self
            .local
            .browser
            .get_selected_files()
            .iter()
            .filter(|f| f.name != "..")
            .map(|f| (f.name.clone(), f.is_dir))
            .collect();

        let local_dir = self.local.browser.current_dir.clone();

        let (session_info, remote_dir, sftp) = {
            if let Some(tab) = self.current_tab() {
                let sftp = match tab.connection.sftp() {
                    Ok(sftp) => sftp,
                    Err(e) => {
                        self.error_message = Some(format!("Cannot start upload: {}", e));
                        return;
                    }
                };
                (
                    tab.session_info.clone(),
                    tab.browser.current_dir.clone(),
                    sftp,
                )
            } else {
                return;
            }
        };

        let mut file_plans = Vec::new();
        let mut remote_directories = HashSet::new();

        for (name, is_dir) in &files {
            let local_path = PathBuf::from(&local_dir).join(name);
            let remote_path = Path::new(&remote_dir).join(name);
            info!("  Upload: {} (is_dir: {})", local_path.display(), is_dir);

            if let Err(e) = Self::collect_local_uploads(
                &local_path,
                &remote_path,
                *is_dir,
                &mut remote_directories,
                &mut file_plans,
            ) {
                self.error_message = Some(format!("Cannot queue upload: {}", e));
                return;
            }
        }

        if let Err(e) = Self::create_remote_directories(&sftp, &remote_directories) {
            self.error_message = Some(format!("Cannot create remote directories: {}", e));
            return;
        }

        let count = file_plans.len();
        info!(
            "Queuing {} file(s) for upload from {} to {}",
            count, local_dir, remote_dir
        );

        if count == 0 {
            self.status_message = Some("Created empty remote directories".to_string());
            self.error_message = None;
            self.local.browser.clear_selection();
            return;
        }

        for plan in file_plans {
            self.transfer_manager.queue_upload_to_path(
                session_info.clone(),
                plan.local_path,
                plan.remote_path,
                false,
                Some(plan.total_bytes),
            );
        }

        self.status_message = Some(format!("Queued {} file(s) for upload", count));
        self.error_message = None;
        self.local.browser.clear_selection();
    }

    fn collect_local_uploads(
        local_path: &Path,
        remote_path: &Path,
        is_dir: bool,
        remote_directories: &mut HashSet<String>,
        plans: &mut Vec<UploadPlanItem>,
    ) -> Result<()> {
        if is_dir {
            remote_directories.insert(remote_path.to_string_lossy().to_string());

            for entry in fs::read_dir(local_path)? {
                let entry = entry?;
                let child_local = entry.path();
                let child_remote = remote_path.join(entry.file_name());
                Self::collect_local_uploads(
                    &child_local,
                    &child_remote,
                    entry.file_type()?.is_dir(),
                    remote_directories,
                    plans,
                )?;
            }

            return Ok(());
        }

        let total_bytes = fs::metadata(local_path)?.len();
        plans.push(UploadPlanItem {
            local_path: local_path.to_string_lossy().to_string(),
            remote_path: remote_path.to_string_lossy().to_string(),
            total_bytes,
        });
        Ok(())
    }

    fn create_remote_directories(sftp: &Sftp, remote_directories: &HashSet<String>) -> Result<()> {
        let mut directories: Vec<_> = remote_directories.iter().map(PathBuf::from).collect();
        directories.sort_by_key(|path| path.components().count());

        for directory in directories {
            Self::create_remote_directory(sftp, &directory)?;
        }

        Ok(())
    }

    fn create_remote_directory(sftp: &Sftp, remote_path: &Path) -> Result<()> {
        if remote_path.as_os_str().is_empty() || remote_path == Path::new("/") {
            return Ok(());
        }

        if sftp.stat(remote_path).is_ok() {
            return Ok(());
        }

        if let Some(parent) = remote_path.parent() {
            if !parent.as_os_str().is_empty() && parent != remote_path {
                Self::create_remote_directory(sftp, parent)?;
            }
        }

        match sftp.mkdir(remote_path, 0o755) {
            Ok(()) => Ok(()),
            Err(_err) if sftp.stat(remote_path).is_ok() => Ok(()),
            Err(err) => Err(err.into()),
        }
    }

    pub fn handle_zip_press(&mut self) {
        let now = Instant::now();
        if let Some(first) = self.zip_awaiting_second_press {
            if now.duration_since(first) < Duration::from_millis(400) {
                self.zip_awaiting_second_press = None;
                self.pending_zip_transfer = true;
                self.zip_selected();
                return;
            }
        }
        self.zip_awaiting_second_press = Some(now);
    }

    pub fn zip_selected(&mut self) {
        match self.focus {
            FocusPanel::Local => self.zip_local_files(),
            FocusPanel::Remote => self.zip_remote_files(),
            FocusPanel::ConnectionTabs => {
                self.zip_awaiting_second_press = None;
                self.pending_zip_transfer = false;
            }
        }
    }

    pub fn extract_selected(&mut self) {
        match self.focus {
            FocusPanel::Local => self.extract_local_archives(),
            FocusPanel::Remote => self.extract_remote_archives(),
            FocusPanel::ConnectionTabs => {}
        }
    }

    fn zip_local_files(&mut self) {
        let base = PathBuf::from(&self.local.browser.current_dir);
        let files: Vec<PathBuf> = self
            .local
            .browser
            .get_selected_files()
            .iter()
            .filter(|f| f.name != "..")
            .map(|f| base.join(&f.name))
            .collect();

        if files.is_empty() {
            self.zip_awaiting_second_press = None;
            self.pending_zip_transfer = false;
            return;
        }

        let zip_name = if files.len() == 1 {
            format!(
                "{}.zip",
                files[0].file_name().unwrap_or_default().to_string_lossy()
            )
        } else {
            format!(
                "archive_{}.zip",
                chrono::Local::now().format("%Y%m%d_%H%M%S")
            )
        };

        let zip_path = base.join(&zip_name);
        info!("Creating local zip: {:?} from {:?}", zip_path, files);

        self.local.browser.clear_selection();

        let should_upload = self.pending_zip_transfer && !self.tabs.is_empty();
        self.pending_zip_transfer = false;

        match create_zip(files.clone(), &base, &zip_path) {
            Ok(()) => {
                info!("Local zip created successfully: {}", zip_name);
                let _ = LocalBrowser::refresh_files(&mut self.local.browser);

                if let Some(idx) = self
                    .local
                    .browser
                    .files
                    .iter()
                    .position(|f| f.name == zip_name)
                {
                    self.local.browser.selected_index = idx;
                    self.local.browser.selected_indices.insert(idx);
                }

                if should_upload {
                    let dest = self
                        .tabs
                        .get(self.active_tab)
                        .map(|t| format!("{}@{}", t.session_info.username, t.session_info.host))
                        .unwrap_or_else(|| "remote".to_string());
                    self.upload_selected();
                    self.status_message =
                        Some(format!("Zipped {} → upload queued to {}", zip_name, dest));
                } else {
                    self.status_message = Some(format!("Created {}", zip_name));
                }
                self.error_message = None;
            }
            Err(e) => {
                error!("Local zip failed: {}", e);
                self.error_message = Some(format!("Zip failed: {}", e));
            }
        }
    }

    fn zip_remote_files(&mut self) {
        let (files, current_dir) = if let Some(tab) = self.current_tab() {
            let files: Vec<String> = tab
                .browser
                .get_selected_files()
                .iter()
                .filter(|f| f.name != "..")
                .map(|f| f.name.clone())
                .collect();
            (files, tab.browser.current_dir.clone())
        } else {
            self.zip_awaiting_second_press = None;
            self.pending_zip_transfer = false;
            return;
        };

        if files.is_empty() {
            self.zip_awaiting_second_press = None;
            self.pending_zip_transfer = false;
            return;
        }

        let zip_name = if files.len() == 1 {
            format!("{}.zip", files[0])
        } else {
            format!(
                "archive_{}.zip",
                chrono::Local::now().format("%Y%m%d_%H%M%S")
            )
        };

        let files_arg = files
            .iter()
            .map(|f| format!("\"{}\"", f))
            .collect::<Vec<_>>()
            .join(" ");

        let cmd = format!(
            "cd \"{}\" && zip -r \"{}\" {} 2>&1",
            current_dir, zip_name, files_arg
        );
        info!("Creating remote zip with command: {}", cmd);

        let should_download = self.pending_zip_transfer;
        self.pending_zip_transfer = false;

        let result = if let Some(tab) = self.current_tab_mut() {
            tab.browser.clear_selection();
            tab.connection.exec(&cmd)
        } else {
            return;
        };

        let mut success = false;

        match result {
            Ok(output) => {
                info!("Remote zip output: {}", output);

                let lower = output.to_lowercase();
                let is_error = lower.contains("command not found")
                    || lower.contains("zip: not found")
                    || lower.contains("no such file")
                    || lower.contains("permission denied")
                    || (lower.contains("zip error") || lower.contains("zip: error"));

                if is_error {
                    warn!("Remote zip failed: {}", output);
                    self.error_message = Some(output.trim().to_string());
                } else {
                    info!("Remote zip created successfully: {}", zip_name);
                    self.error_message = None;
                    success = true;
                }
            }
            Err(e) => {
                error!("Remote zip command failed: {}", e);
                self.error_message = Some(format!("Zip failed: {}", e));
            }
        }

        if let Some(tab) = self.current_tab_mut() {
            let _ = tab.refresh_directory();

            if success {
                if let Some(idx) = tab.browser.files.iter().position(|f| f.name == zip_name) {
                    tab.browser.selected_index = idx;
                    tab.browser.selected_indices.insert(idx);
                }
            }
        }

        if success && should_download {
            let local_dir = self.local.browser.current_dir.clone();
            self.download_selected();
            self.status_message = Some(format!(
                "Zipped {} → download queued to {}",
                zip_name, local_dir
            ));
        } else if success {
            self.status_message = Some(format!("Created {}", zip_name));
        }
    }

    fn extract_local_archives(&mut self) {
        let current_dir = self.local.browser.current_dir.clone();
        let selected = self.local.browser.get_selected_files();
        let archives = match Self::extractable_zip_names(selected) {
            Ok(archives) => archives,
            Err(message) => {
                self.error_message = Some(message.to_string());
                self.status_message = None;
                return;
            }
        };

        if archives.is_empty() {
            return;
        }

        match self.local_extract_conflicts(&current_dir, &archives) {
            Ok(conflicts) if !conflicts.is_empty() => {
                self.show_extract_conflict_dialog(
                    FocusPanel::Local,
                    current_dir,
                    archives,
                    conflicts,
                );
                return;
            }
            Ok(_) => {}
            Err(e) => {
                error!("Local unzip conflict scan failed: {}", e);
                self.error_message = Some(format!("Extract failed: {}", e));
                self.status_message = None;
                return;
            }
        }

        self.perform_local_extract(&current_dir, &archives, ExtractConflictStrategy::Overwrite);
    }

    fn extract_remote_archives(&mut self) {
        let (archives, current_dir) = if let Some(tab) = self.current_tab() {
            let selected = tab.browser.get_selected_files();
            let archives = match Self::extractable_zip_names(selected) {
                Ok(archives) => archives,
                Err(message) => {
                    self.error_message = Some(message.to_string());
                    self.status_message = None;
                    return;
                }
            };
            (archives, tab.browser.current_dir.clone())
        } else {
            return;
        };

        if archives.is_empty() {
            return;
        }

        match self.remote_extract_conflicts(&current_dir, &archives) {
            Ok(conflicts) if !conflicts.is_empty() => {
                self.show_extract_conflict_dialog(
                    FocusPanel::Remote,
                    current_dir,
                    archives,
                    conflicts,
                );
                return;
            }
            Ok(_) => {}
            Err(e) => {
                error!("Remote unzip conflict scan failed: {}", e);
                self.error_message = Some(format!("Extract failed: {}", e));
                self.status_message = None;
                return;
            }
        }

        self.perform_remote_extract(&current_dir, &archives, ExtractConflictStrategy::Overwrite);
    }

    fn local_extract_conflicts(
        &self,
        current_dir: &str,
        archives: &[String],
    ) -> Result<Vec<String>> {
        let base = PathBuf::from(current_dir);
        let mut conflicts = HashSet::new();

        for archive_name in archives {
            let archive_path = base.join(archive_name);
            for relative in list_zip_entries(&archive_path)? {
                if base.join(&relative).exists() {
                    conflicts.insert(relative.to_string_lossy().to_string());
                }
            }
        }

        let mut conflicts: Vec<_> = conflicts.into_iter().collect();
        conflicts.sort();
        Ok(conflicts)
    }

    fn remote_extract_conflicts(
        &mut self,
        current_dir: &str,
        archives: &[String],
    ) -> Result<Vec<String>> {
        let mut conflicts = HashSet::new();

        for archive_name in archives {
            let cmd = format!(
                "status=0; cd {} || status=$?; if [ \"$status\" -eq 0 ]; then entries=$(unzip -Z1 {} 2>&1); status=$?; if [ \"$status\" -eq 0 ]; then printf '%s\\n' \"$entries\" | while IFS= read -r entry; do [ -z \"$entry\" ] && continue; case \"$entry\" in */) continue ;; esac; [ -e \"$entry\" ] && printf '%s\\n' \"$entry\"; done; else printf '%s\\n' \"$entries\"; fi; fi; printf '\\n__BADASSH_STATUS__%s' \"$status\"",
                Self::shell_quote(current_dir),
                Self::shell_quote(archive_name),
            );

            let output = if let Some(tab) = self.current_tab_mut() {
                tab.connection.exec(&cmd)?
            } else {
                anyhow::bail!("No active remote tab");
            };

            let (message, status) = Self::split_remote_status_marker(&output);
            if status != Some(0) {
                anyhow::bail!(if message.is_empty() {
                    format!("Failed to inspect {}", archive_name)
                } else {
                    message
                });
            }

            for line in message
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
            {
                conflicts.insert(line.to_string());
            }
        }

        let mut conflicts: Vec<_> = conflicts.into_iter().collect();
        conflicts.sort();
        Ok(conflicts)
    }

    fn perform_local_extract(
        &mut self,
        current_dir: &str,
        archives: &[String],
        strategy: ExtractConflictStrategy,
    ) {
        let base = PathBuf::from(current_dir);

        for archive_name in archives {
            let archive_path = base.join(archive_name);
            if let Err(e) = extract_zip_archive(&archive_path, &base, &strategy) {
                error!("Local unzip failed for {}: {}", archive_name, e);
                let _ = LocalBrowser::refresh_files(&mut self.local.browser);
                self.error_message = Some(format!("Extract failed: {}", e));
                self.status_message = None;
                return;
            }
        }

        let _ = LocalBrowser::refresh_files(&mut self.local.browser);
        self.error_message = None;
        self.status_message = Some(if archives.len() == 1 {
            format!("Extracted {}", archives[0])
        } else {
            format!("Extracted {} zip files", archives.len())
        });
    }

    fn perform_remote_extract(
        &mut self,
        current_dir: &str,
        archives: &[String],
        strategy: ExtractConflictStrategy,
    ) {
        let mut success = true;

        for archive_name in archives {
            let cmd = match &strategy {
                ExtractConflictStrategy::Overwrite => {
                    self.build_remote_extract_overwrite_command(current_dir, archive_name)
                }
                ExtractConflictStrategy::KeepBoth { timestamp } => self
                    .build_remote_extract_keep_both_command(current_dir, archive_name, timestamp),
            };
            info!("Extracting remote zip with command: {}", cmd);

            let output = if let Some(tab) = self.current_tab_mut() {
                match tab.connection.exec(&cmd) {
                    Ok(output) => output,
                    Err(e) => {
                        error!("Remote unzip command failed: {}", e);
                        self.error_message = Some(format!("Extract failed: {}", e));
                        self.status_message = None;
                        success = false;
                        break;
                    }
                }
            } else {
                self.error_message = Some("No active remote tab".to_string());
                self.status_message = None;
                success = false;
                break;
            };

            let (message, status) = Self::split_remote_status_marker(&output);
            if status != Some(0) {
                let message = if message.is_empty() {
                    format!("Failed to extract {}", archive_name)
                } else {
                    message
                };
                warn!("Remote unzip failed: {}", message);
                self.error_message = Some(message);
                self.status_message = None;
                success = false;
                break;
            }
        }

        if let Some(tab) = self.current_tab_mut() {
            let _ = tab.refresh_directory();
        }

        if success {
            self.error_message = None;
            self.status_message = Some(if archives.len() == 1 {
                format!("Extracted {}", archives[0])
            } else {
                format!("Extracted {} zip files", archives.len())
            });
        }
    }

    fn extractable_zip_names<'a>(
        selected: impl IntoIterator<Item = &'a FileEntry>,
    ) -> std::result::Result<Vec<String>, &'static str> {
        let mut archives = Vec::new();

        for entry in selected {
            if entry.name == ".." {
                continue;
            }

            if entry.is_dir || !Self::is_zip_file_name(&entry.name) {
                return Err("Can only extract zip files");
            }

            archives.push(entry.name.clone());
        }

        Ok(archives)
    }

    fn is_zip_file_name(name: &str) -> bool {
        name.to_ascii_lowercase().ends_with(".zip")
    }

    fn shell_quote(value: &str) -> String {
        format!("'{}'", value.replace('\'', "'\"'\"'"))
    }

    fn build_remote_extract_overwrite_command(
        &self,
        current_dir: &str,
        archive_name: &str,
    ) -> String {
        format!(
            "status=0; cd {} || status=$?; if [ \"$status\" -eq 0 ]; then output=$(unzip -o {} 2>&1); status=$?; if [ -n \"$output\" ]; then printf '%s' \"$output\"; fi; fi; printf '\\n__BADASSH_STATUS__%s' \"$status\"",
            Self::shell_quote(current_dir),
            Self::shell_quote(archive_name),
        )
    }

    fn build_remote_extract_keep_both_command(
        &self,
        current_dir: &str,
        archive_name: &str,
        timestamp: &str,
    ) -> String {
        let nl = "'\n'";

        format!(
            "status=0; cd {dir} || status=$?; tmp=''; if [ \"$status\" -eq 0 ]; then tmp=$(mktemp -d .badassh_extract_XXXXXX 2>&1); status=$?; fi; if [ \"$status\" -eq 0 ]; then output=$(unzip -o {archive} -d \"$tmp\" 2>&1); status=$?; if [ -n \"$output\" ]; then printf '%s\\n' \"$output\"; fi; fi; if [ \"$status\" -eq 0 ]; then OLDIFS=$IFS; IFS={nl}; for dir_path in $(find \"$tmp\" -mindepth 1 -type d 2>/dev/null); do rel=${{dir_path#\"$tmp\"/}}; mkdir -p \"./$rel\" || {{ status=$?; break; }}; done; IFS=$OLDIFS; fi; if [ \"$status\" -eq 0 ]; then OLDIFS=$IFS; IFS={nl}; for src in $(find \"$tmp\" \\( -type f -o -type l \\) 2>/dev/null); do rel=${{src#\"$tmp\"/}}; dest=\"./$rel\"; dest_dir=$(dirname \"$dest\"); mkdir -p \"$dest_dir\" || {{ status=$?; break; }}; if [ -e \"$dest\" ]; then base=${{dest##*/}}; dir_part=${{dest%/*}}; [ \"$dir_part\" = \"$dest\" ] && dir_part='.'; case \"$base\" in *.*) stem=${{base%.*}}; ext=${{base##*.}}; candidate=\"$dir_part/$stem.{timestamp}.$ext\" ;; *) candidate=\"$dir_part/$base.{timestamp}\" ;; esac; n=2; while [ -e \"$candidate\" ]; do case \"$base\" in *.*) candidate=\"$dir_part/$stem.{timestamp}.$n.$ext\" ;; *) candidate=\"$dir_part/$base.{timestamp}.$n\" ;; esac; n=$((n + 1)); done; mv \"$src\" \"$candidate\" || {{ status=$?; break; }}; else mv \"$src\" \"$dest\" || {{ status=$?; break; }}; fi; done; IFS=$OLDIFS; fi; if [ -n \"$tmp\" ] && [ \"$tmp\" != '.' ]; then rm -rf \"$tmp\"; fi; printf '\\n__BADASSH_STATUS__%s' \"$status\"",
            dir = Self::shell_quote(current_dir),
            archive = Self::shell_quote(archive_name),
            timestamp = timestamp,
            nl = nl,
        )
    }

    fn split_remote_status_marker(output: &str) -> (String, Option<i32>) {
        const MARKER: &str = "__BADASSH_STATUS__";

        if let Some(idx) = output.rfind(MARKER) {
            let message = output[..idx].trim().to_string();
            let status = output[idx + MARKER.len()..].trim().parse::<i32>().ok();
            (message, status)
        } else {
            (output.trim().to_string(), None)
        }
    }

    /// Open selected files in the default editor
    pub fn open_selected(&mut self) {
        match self.focus {
            FocusPanel::Local => {
                self.open_local_files();
            }
            FocusPanel::Remote => {
                self.open_remote_files();
            }
            FocusPanel::ConnectionTabs => {}
        }
    }

    fn open_local_files(&mut self) {
        let all_selected = self.local.browser.get_selected_files();
        debug!(
            "open_local_files: {} files in selection",
            all_selected.len()
        );

        let files: Vec<(String, bool)> = all_selected
            .iter()
            .filter(|f| f.name != ".." && !f.is_dir)
            .map(|f| (f.name.clone(), f.is_dir))
            .collect();

        if files.is_empty() {
            debug!("No files to open (all filtered out or empty selection)");
            return;
        }

        let current_dir = self.local.browser.current_dir.clone();
        let editor_command = self.preferences.editor_command.clone();

        for (name, _is_dir) in files {
            let path = PathBuf::from(&current_dir).join(&name);
            info!(
                "Opening local file: {:?} with editor: {}",
                path, editor_command
            );
            match self.editor_manager.open_local_file(&path, &editor_command) {
                Ok(()) => {
                    info!("Opened local file: {:?}", path);
                    self.status_message = Some(format!("Opened {}", name));
                }
                Err(e) => {
                    error!("Failed to open file {}: {}", name, e);
                    self.error_message = Some(format!("Failed to open {}: {}", name, e));
                    return;
                }
            }
        }

        self.error_message = None;
    }

    fn open_remote_files(&mut self) {
        // Get selected files and connection info
        let (files, current_dir, session_info) = if let Some(tab) = self.current_tab() {
            let files: Vec<String> = tab
                .browser
                .get_selected_files()
                .iter()
                .filter(|f| f.name != ".." && !f.is_dir)
                .map(|f| f.name.clone())
                .collect();
            let session_info = tab.session_info.clone();
            (files, tab.browser.current_dir.clone(), session_info)
        } else {
            return;
        };

        if files.is_empty() {
            return;
        }

        // Get SFTP from connection
        let sftp = match self
            .current_tab()
            .and_then(|tab| tab.connection.sftp().ok())
        {
            Some(sftp) => sftp,
            None => {
                self.error_message = Some("Failed to create SFTP session".to_string());
                return;
            }
        };

        let editor_command = self.preferences.editor_command.clone();

        for name in files {
            let remote_path = if current_dir.ends_with('/') {
                format!("{}{}", current_dir, name)
            } else {
                format!("{}/{}", current_dir, name)
            };

            match self.editor_manager.open_remote_file(
                &session_info,
                &remote_path,
                &sftp,
                &editor_command,
            ) {
                Ok(()) => {
                    info!("Opened remote file: {}", remote_path);
                    self.status_message = Some(format!("Opened {}", name));
                }
                Err(e) => {
                    error!("Failed to open remote file {}: {}", name, e);
                    self.error_message = Some(format!("Failed to open {}: {}", name, e));
                    return;
                }
            }
        }

        self.error_message = None;
    }

    pub fn enter_selected(&mut self) -> Result<()> {
        match self.focus {
            FocusPanel::Local => {
                if let Some(file) = self.local.browser.selected_file().cloned() {
                    if file.is_dir {
                        LocalBrowser::change_directory(&mut self.local.browser, &file.name)?;
                        self.update_local_watcher();
                        self.error_message = None;
                        self.status_message = None;
                        return Ok(());
                    }

                    // Open file in editor
                    self.open_selected();
                }
            }
            FocusPanel::Remote => {
                let selected_file = self
                    .current_tab()
                    .and_then(|tab| tab.browser.selected_file().cloned());

                if let Some(file) = selected_file {
                    if file.is_dir {
                        if let Some(tab) = self.current_tab_mut() {
                            tab.change_directory(&file.name)?;
                        }
                        self.error_message = None;
                        self.status_message = None;
                        return Ok(());
                    }

                    self.open_selected();
                }
            }
            FocusPanel::ConnectionTabs => {}
        }
        // Clear status and error on successful navigation
        self.error_message = None;
        self.status_message = None;
        Ok(())
    }

    pub fn show_delete_confirm(&mut self) {
        let targets: Vec<(String, bool)> = match self.focus {
            FocusPanel::ConnectionTabs => return,
            FocusPanel::Local => self
                .local
                .browser
                .get_selected_files()
                .iter()
                .filter(|f| f.name != "..")
                .map(|f| (f.name.clone(), f.is_dir))
                .collect(),
            FocusPanel::Remote => {
                if let Some(tab) = self.current_tab() {
                    tab.browser
                        .get_selected_files()
                        .iter()
                        .filter(|f| f.name != "..")
                        .map(|f| (f.name.clone(), f.is_dir))
                        .collect()
                } else {
                    return;
                }
            }
        };

        if targets.is_empty() {
            return;
        }

        self.pending_delete_operation = Some(PendingDeleteOperation::Files(targets));
        self.delete_confirm_yes = true;
        self.mode = AppMode::DeleteConfirm;
    }

    pub fn show_delete_saved_connection_confirm(&mut self) {
        let Some(saved) = self
            .recent_connections
            .get(self.connection_list_index)
            .cloned()
        else {
            return;
        };

        self.pending_delete_operation = Some(PendingDeleteOperation::SavedConnection(saved));
        self.delete_confirm_yes = true;
        self.mode = AppMode::DeleteConfirm;
    }

    fn show_extract_conflict_dialog(
        &mut self,
        target: FocusPanel,
        current_dir: String,
        archives: Vec<String>,
        conflict_paths: Vec<String>,
    ) {
        self.extract_conflict_overwrite = true;
        self.pending_extract_operation = Some(PendingExtractOperation {
            target,
            current_dir,
            archives,
            conflict_paths,
        });
        self.mode = AppMode::ExtractConflictConfirm;
    }

    pub fn cancel_extract_conflict(&mut self) {
        self.pending_extract_operation = None;
        self.extract_conflict_overwrite = true;
        self.mode = if !self.tabs.is_empty() {
            AppMode::Connected
        } else {
            AppMode::Normal
        };
    }

    pub fn toggle_extract_conflict_option(&mut self) {
        self.extract_conflict_overwrite = !self.extract_conflict_overwrite;
    }

    pub fn confirm_extract_conflict(&mut self) {
        let Some(op) = self.pending_extract_operation.take() else {
            self.cancel_extract_conflict();
            return;
        };

        let strategy = if self.extract_conflict_overwrite {
            ExtractConflictStrategy::Overwrite
        } else {
            ExtractConflictStrategy::KeepBoth {
                timestamp: chrono::Local::now().format("%Y%m%d%H%M%S").to_string(),
            }
        };

        self.extract_conflict_overwrite = true;
        self.mode = if !self.tabs.is_empty() {
            AppMode::Connected
        } else {
            AppMode::Normal
        };

        match op.target {
            FocusPanel::Local => {
                self.perform_local_extract(&op.current_dir, &op.archives, strategy)
            }
            FocusPanel::Remote => {
                self.perform_remote_extract(&op.current_dir, &op.archives, strategy)
            }
            FocusPanel::ConnectionTabs => {}
        }
    }

    pub fn cancel_delete(&mut self) {
        self.mode = if !self.tabs.is_empty() {
            AppMode::Connected
        } else {
            AppMode::Normal
        };
        self.pending_delete_operation = None;
    }

    pub fn toggle_delete_option(&mut self) {
        self.delete_confirm_yes = !self.delete_confirm_yes;
    }

    pub fn format_saved_connection_label(saved: &SavedConnection) -> String {
        format!(
            "{} ({}@{}:{})",
            saved.name, saved.username, saved.host, saved.port
        )
    }

    fn clamp_recent_connection_panel_index(&mut self) {
        let total_items = self.recent_connections.len() + 1;
        self.connection_list_index = self
            .connection_list_index
            .min(total_items.saturating_sub(1));
    }

    pub fn confirm_delete(&mut self) {
        if !self.delete_confirm_yes {
            self.cancel_delete();
            return;
        }

        let Some(operation) = self.pending_delete_operation.take() else {
            self.cancel_delete();
            return;
        };

        match operation {
            PendingDeleteOperation::Files(targets) => {
                if targets.is_empty() {
                    self.cancel_delete();
                    return;
                }

                let panel = match self.focus {
                    FocusPanel::Local => "local",
                    FocusPanel::Remote => "remote",
                    FocusPanel::ConnectionTabs => {
                        self.cancel_delete();
                        return;
                    }
                };

                let mut failures: Vec<(String, String)> = Vec::new();
                for (name, is_dir) in &targets {
                    info!("Deleting {} \"{}\" (is_dir: {})", panel, name, is_dir);
                    let one: Result<(), String> = match self.focus {
                        FocusPanel::Local => {
                            let path = PathBuf::from(&self.local.browser.current_dir).join(name);
                            debug!("Local delete path: {:?}", path);
                            if *is_dir {
                                std::fs::remove_dir_all(&path)
                            } else {
                                std::fs::remove_file(&path)
                            }
                            .map_err(|e| e.to_string())
                        }
                        FocusPanel::Remote => {
                            if let Some(tab) = self.current_tab_mut() {
                                let remote_path = format!(
                                    "{}/{}",
                                    tab.browser.current_dir.trim_end_matches('/'),
                                    name
                                );
                                let cmd = if *is_dir {
                                    format!("rm -rf \"{}\"", remote_path)
                                } else {
                                    format!("rm -f \"{}\"", remote_path)
                                };
                                debug!("Remote delete command: {}", cmd);
                                tab.connection
                                    .exec(&cmd)
                                    .map(|_| ())
                                    .map_err(|e| e.to_string())
                            } else {
                                Ok(())
                            }
                        }
                        FocusPanel::ConnectionTabs => Ok(()),
                    };
                    if let Err(e) = one {
                        failures.push((name.clone(), e));
                    }
                }

                match self.focus {
                    FocusPanel::Local => {
                        let _ = LocalBrowser::refresh_files(&mut self.local.browser);
                    }
                    FocusPanel::Remote => {
                        if let Some(tab) = self.current_tab_mut() {
                            let _ = tab.refresh_directory();
                        }
                    }
                    FocusPanel::ConnectionTabs => {}
                }

                let n_ok = targets.len() - failures.len();
                if failures.is_empty() {
                    self.error_message = None;
                    self.status_message = Some(if targets.len() == 1 {
                        format!("Deleted \"{}\"", targets[0].0)
                    } else {
                        format!("Deleted {} items", targets.len())
                    });
                } else if n_ok == 0 {
                    self.error_message = Some(format!("Delete failed: {}", failures[0].1));
                    self.status_message = None;
                } else {
                    self.error_message = Some(format!(
                        "Deleted {}, {} failed (e.g. {}: {})",
                        n_ok,
                        failures.len(),
                        failures[0].0,
                        failures[0].1
                    ));
                    self.status_message = None;
                }
            }
            PendingDeleteOperation::SavedConnection(saved) => {
                let label = Self::format_saved_connection_label(&saved);
                match self.db.delete_connection(&saved.id) {
                    Ok(()) => {
                        self.refresh_connections();
                        self.clamp_recent_connection_panel_index();
                        self.error_message = None;
                        self.status_message = Some(format!("Deleted connection {}", label));
                    }
                    Err(e) => {
                        self.error_message = Some(format!("Delete failed: {}", e));
                        self.status_message = None;
                    }
                }
            }
        }

        self.cancel_delete();
    }
}
