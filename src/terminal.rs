use anyhow::{Context, Result};
use log::{debug, error, info};
use parking_lot::Mutex;
use portable_pty::{native_pty_system, CommandBuilder, PtyPair, PtySize};
use ssh2::{Channel, Session};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU16, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;
use vte::{Params, Parser, Perform};

use crate::transfer::SftpSessionInfo;

const MAX_SCROLLBACK: usize = 5000;
const DEFAULT_COLS: u16 = 80;
const DEFAULT_ROWS: u16 = 24;

#[derive(Clone)]
struct ScreenCell {
    c: char,
}

impl Default for ScreenCell {
    fn default() -> Self {
        Self { c: ' ' }
    }
}

struct TerminalScreen {
    cells: Vec<Vec<ScreenCell>>,
    scrollback: Vec<Vec<ScreenCell>>,
    cursor_x: usize,
    cursor_y: usize,
    cols: usize,
    rows: usize,
    scroll_top: usize,
    scroll_bottom: usize,
    saved_cursor: Option<(usize, usize)>,
}

impl TerminalScreen {
    fn new(cols: usize, rows: usize) -> Self {
        let cells = vec![vec![ScreenCell::default(); cols]; rows];
        Self {
            cells,
            scrollback: Vec::new(),
            cursor_x: 0,
            cursor_y: 0,
            cols,
            rows,
            scroll_top: 0,
            scroll_bottom: rows.saturating_sub(1),
            saved_cursor: None,
        }
    }

    fn resize(&mut self, cols: usize, rows: usize) {
        if cols == self.cols && rows == self.rows {
            return;
        }
        
        self.cols = cols;
        self.rows = rows;
        self.scroll_bottom = rows.saturating_sub(1);
        
        self.cells.resize(rows, vec![ScreenCell::default(); cols]);
        for row in &mut self.cells {
            row.resize(cols, ScreenCell::default());
        }
        
        self.cursor_x = self.cursor_x.min(cols.saturating_sub(1));
        self.cursor_y = self.cursor_y.min(rows.saturating_sub(1));
    }

    fn put_char(&mut self, c: char) {
        if self.cursor_x >= self.cols {
            self.cursor_x = 0;
            self.newline();
        }
        
        if self.cursor_y < self.rows && self.cursor_x < self.cols {
            self.cells[self.cursor_y][self.cursor_x].c = c;
            self.cursor_x += 1;
        }
    }

    fn newline(&mut self) {
        if self.cursor_y >= self.scroll_bottom {
            self.scroll_up(1);
        } else {
            self.cursor_y += 1;
        }
    }

    fn carriage_return(&mut self) {
        self.cursor_x = 0;
    }

    fn backspace(&mut self) {
        if self.cursor_x > 0 {
            self.cursor_x -= 1;
        }
    }

    fn tab(&mut self) {
        let next_tab = ((self.cursor_x / 8) + 1) * 8;
        self.cursor_x = next_tab.min(self.cols.saturating_sub(1));
    }

    fn scroll_up(&mut self, count: usize) {
        for _ in 0..count {
            if self.scroll_top < self.cells.len() {
                let row = self.cells.remove(self.scroll_top);
                if self.scrollback.len() >= MAX_SCROLLBACK {
                    self.scrollback.remove(0);
                }
                self.scrollback.push(row);
            }
            
            let bottom = self.scroll_bottom.min(self.rows.saturating_sub(1));
            if bottom < self.rows {
                self.cells.insert(bottom, vec![ScreenCell::default(); self.cols]);
            }
        }
        
        while self.cells.len() > self.rows {
            self.cells.pop();
        }
        while self.cells.len() < self.rows {
            self.cells.push(vec![ScreenCell::default(); self.cols]);
        }
    }

    fn scroll_down(&mut self, count: usize) {
        for _ in 0..count {
            let bottom = self.scroll_bottom.min(self.rows.saturating_sub(1));
            if bottom < self.cells.len() {
                self.cells.remove(bottom);
            }
            self.cells.insert(self.scroll_top, vec![ScreenCell::default(); self.cols]);
        }
        
        while self.cells.len() > self.rows {
            self.cells.pop();
        }
    }

    fn clear_screen(&mut self, mode: u16) {
        match mode {
            0 => {
                for x in self.cursor_x..self.cols {
                    if self.cursor_y < self.rows {
                        self.cells[self.cursor_y][x].c = ' ';
                    }
                }
                for y in (self.cursor_y + 1)..self.rows {
                    for x in 0..self.cols {
                        self.cells[y][x].c = ' ';
                    }
                }
            }
            1 => {
                for y in 0..self.cursor_y {
                    for x in 0..self.cols {
                        self.cells[y][x].c = ' ';
                    }
                }
                for x in 0..=self.cursor_x.min(self.cols.saturating_sub(1)) {
                    if self.cursor_y < self.rows {
                        self.cells[self.cursor_y][x].c = ' ';
                    }
                }
            }
            2 | 3 => {
                for row in &mut self.cells {
                    for cell in row {
                        cell.c = ' ';
                    }
                }
            }
            _ => {}
        }
    }

    fn clear_line(&mut self, mode: u16) {
        if self.cursor_y >= self.rows {
            return;
        }
        match mode {
            0 => {
                for x in self.cursor_x..self.cols {
                    self.cells[self.cursor_y][x].c = ' ';
                }
            }
            1 => {
                for x in 0..=self.cursor_x.min(self.cols.saturating_sub(1)) {
                    self.cells[self.cursor_y][x].c = ' ';
                }
            }
            2 => {
                for x in 0..self.cols {
                    self.cells[self.cursor_y][x].c = ' ';
                }
            }
            _ => {}
        }
    }

    fn delete_chars(&mut self, count: usize) {
        if self.cursor_y >= self.rows {
            return;
        }
        let row = &mut self.cells[self.cursor_y];
        for _ in 0..count {
            if self.cursor_x < row.len() {
                row.remove(self.cursor_x);
                row.push(ScreenCell::default());
            }
        }
    }

    fn insert_chars(&mut self, count: usize) {
        if self.cursor_y >= self.rows {
            return;
        }
        let row = &mut self.cells[self.cursor_y];
        for _ in 0..count {
            if self.cursor_x < row.len() {
                row.insert(self.cursor_x, ScreenCell::default());
                row.truncate(self.cols);
            }
        }
    }

    fn get_lines(&self) -> Vec<String> {
        self.cells
            .iter()
            .map(|row| row.iter().map(|c| c.c).collect::<String>().trim_end().to_string())
            .collect()
    }

    fn get_lines_with_scrollback(&self, visible_rows: usize, scroll_offset: usize) -> Vec<String> {
        let total_scrollback = self.scrollback.len();
        
        if scroll_offset == 0 {
            return self.get_lines();
        }
        
        let mut lines = Vec::with_capacity(visible_rows);
        let scroll_start = total_scrollback.saturating_sub(scroll_offset);
        
        for i in scroll_start..total_scrollback {
            if lines.len() >= visible_rows {
                break;
            }
            lines.push(
                self.scrollback[i]
                    .iter()
                    .map(|c| c.c)
                    .collect::<String>()
                    .trim_end()
                    .to_string(),
            );
        }
        
        let remaining = visible_rows.saturating_sub(lines.len());
        for row in self.cells.iter().take(remaining) {
            lines.push(row.iter().map(|c| c.c).collect::<String>().trim_end().to_string());
        }
        
        lines
    }
}

impl Perform for TerminalScreen {
    fn print(&mut self, c: char) {
        self.put_char(c);
    }

    fn execute(&mut self, byte: u8) {
        match byte {
            0x07 => {}
            0x08 => self.backspace(),
            0x09 => self.tab(),
            0x0A | 0x0B | 0x0C => self.newline(),
            0x0D => self.carriage_return(),
            _ => {}
        }
    }

    fn hook(&mut self, _params: &Params, _intermediates: &[u8], _ignore: bool, _action: char) {}
    fn put(&mut self, _byte: u8) {}
    fn unhook(&mut self) {}
    fn osc_dispatch(&mut self, _params: &[&[u8]], _bell_terminated: bool) {}
    fn esc_dispatch(&mut self, _intermediates: &[u8], _ignore: bool, byte: u8) {
        match byte {
            b'7' => self.saved_cursor = Some((self.cursor_x, self.cursor_y)),
            b'8' => {
                if let Some((x, y)) = self.saved_cursor {
                    self.cursor_x = x.min(self.cols.saturating_sub(1));
                    self.cursor_y = y.min(self.rows.saturating_sub(1));
                }
            }
            b'D' => self.newline(),
            b'M' => {
                if self.cursor_y == self.scroll_top {
                    self.scroll_down(1);
                } else if self.cursor_y > 0 {
                    self.cursor_y -= 1;
                }
            }
            b'E' => {
                self.carriage_return();
                self.newline();
            }
            _ => {}
        }
    }

    fn csi_dispatch(&mut self, params: &Params, _intermediates: &[u8], _ignore: bool, action: char) {
        let params: Vec<u16> = params.iter().map(|p| p.first().copied().unwrap_or(0)).collect();
        let param = |i: usize, default: u16| params.get(i).copied().unwrap_or(default).max(1);

        match action {
            'A' => self.cursor_y = self.cursor_y.saturating_sub(param(0, 1) as usize),
            'B' | 'e' => {
                self.cursor_y = (self.cursor_y + param(0, 1) as usize).min(self.rows.saturating_sub(1))
            }
            'C' | 'a' => {
                self.cursor_x = (self.cursor_x + param(0, 1) as usize).min(self.cols.saturating_sub(1))
            }
            'D' => self.cursor_x = self.cursor_x.saturating_sub(param(0, 1) as usize),
            'E' => {
                self.cursor_x = 0;
                self.cursor_y = (self.cursor_y + param(0, 1) as usize).min(self.rows.saturating_sub(1));
            }
            'F' => {
                self.cursor_x = 0;
                self.cursor_y = self.cursor_y.saturating_sub(param(0, 1) as usize);
            }
            'G' | '`' => {
                self.cursor_x = (params.first().copied().unwrap_or(1).saturating_sub(1) as usize)
                    .min(self.cols.saturating_sub(1))
            }
            'H' | 'f' => {
                let row = params.first().copied().unwrap_or(1).saturating_sub(1) as usize;
                let col = params.get(1).copied().unwrap_or(1).saturating_sub(1) as usize;
                self.cursor_y = row.min(self.rows.saturating_sub(1));
                self.cursor_x = col.min(self.cols.saturating_sub(1));
            }
            'J' => self.clear_screen(params.first().copied().unwrap_or(0)),
            'K' => self.clear_line(params.first().copied().unwrap_or(0)),
            'L' => {
                let count = param(0, 1) as usize;
                for _ in 0..count {
                    if self.cursor_y < self.rows {
                        self.cells.insert(self.cursor_y, vec![ScreenCell::default(); self.cols]);
                        let bottom = self.scroll_bottom.min(self.rows.saturating_sub(1));
                        if bottom < self.cells.len() {
                            self.cells.remove(bottom + 1);
                        }
                    }
                }
                while self.cells.len() > self.rows {
                    self.cells.pop();
                }
            }
            'M' => {
                let count = param(0, 1) as usize;
                for _ in 0..count {
                    if self.cursor_y < self.cells.len() {
                        self.cells.remove(self.cursor_y);
                        let bottom = self.scroll_bottom.min(self.rows.saturating_sub(1));
                        self.cells.insert(bottom, vec![ScreenCell::default(); self.cols]);
                    }
                }
            }
            'P' => self.delete_chars(param(0, 1) as usize),
            '@' => self.insert_chars(param(0, 1) as usize),
            'S' => self.scroll_up(param(0, 1) as usize),
            'T' => self.scroll_down(param(0, 1) as usize),
            'X' => {
                let count = param(0, 1) as usize;
                for i in 0..count {
                    let x = self.cursor_x + i;
                    if x < self.cols && self.cursor_y < self.rows {
                        self.cells[self.cursor_y][x].c = ' ';
                    }
                }
            }
            'd' => {
                let row = params.first().copied().unwrap_or(1).saturating_sub(1) as usize;
                self.cursor_y = row.min(self.rows.saturating_sub(1));
            }
            'r' => {
                let top = params.first().copied().unwrap_or(1).saturating_sub(1) as usize;
                let bottom = params.get(1).copied().unwrap_or(self.rows as u16).saturating_sub(1) as usize;
                self.scroll_top = top.min(self.rows.saturating_sub(1));
                self.scroll_bottom = bottom.min(self.rows.saturating_sub(1));
                self.cursor_x = 0;
                self.cursor_y = 0;
            }
            's' => self.saved_cursor = Some((self.cursor_x, self.cursor_y)),
            'u' => {
                if let Some((x, y)) = self.saved_cursor {
                    self.cursor_x = x.min(self.cols.saturating_sub(1));
                    self.cursor_y = y.min(self.rows.saturating_sub(1));
                }
            }
            'm' | 'h' | 'l' | 'n' | 'c' | 'q' | 't' => {}
            _ => {}
        }
    }
}

pub struct LocalTerminal {
    #[allow(dead_code)]
    pty_pair: PtyPair,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    screen: Arc<Mutex<TerminalScreen>>,
    scroll_offset: usize,
    running: Arc<AtomicBool>,
    cols: Arc<AtomicU16>,
    rows: Arc<AtomicU16>,
    pending_resize: Arc<AtomicBool>,
}

impl LocalTerminal {
    pub fn new() -> Result<Self> {
        Self::new_with_size(DEFAULT_COLS, DEFAULT_ROWS)
    }

    pub fn new_with_size(cols: u16, rows: u16) -> Result<Self> {
        let pty_system = native_pty_system();

        let pty_pair = pty_system
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("Failed to open PTY")?;

        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".to_string());
        info!("Starting local terminal with shell: {} ({}x{})", shell, cols, rows);

        let zdotdir = std::env::temp_dir().join("badassh-zsh");
        let _ = std::fs::create_dir_all(&zdotdir);
        let zshrc = zdotdir.join(".zshrc");
        if !zshrc.exists() {
            let _ = std::fs::write(&zshrc, "# minimal zshrc\nsetopt PROMPT_SUBST\nPS1='%~ $ '\n");
        }

        let mut cmd = CommandBuilder::new(&shell);
        cmd.env("TERM", "xterm-256color");
        if shell.contains("zsh") {
            cmd.env("ZDOTDIR", zdotdir.to_string_lossy().to_string());
        }

        if let Ok(cwd) = std::env::current_dir() {
            cmd.cwd(cwd);
        }

        let _child = pty_pair
            .slave
            .spawn_command(cmd)
            .context("Failed to spawn shell")?;

        let writer = pty_pair.master.take_writer().context("Failed to get PTY writer")?;
        let mut reader = pty_pair.master.try_clone_reader().context("Failed to get PTY reader")?;

        let screen = Arc::new(Mutex::new(TerminalScreen::new(cols as usize, rows as usize)));
        let running = Arc::new(AtomicBool::new(true));
        let cols_atomic = Arc::new(AtomicU16::new(cols));
        let rows_atomic = Arc::new(AtomicU16::new(rows));
        let pending_resize = Arc::new(AtomicBool::new(false));

        let screen_clone = screen.clone();
        let running_clone = running.clone();
        let cols_clone = cols_atomic.clone();
        let rows_clone = rows_atomic.clone();
        let pending_resize_clone = pending_resize.clone();

        thread::spawn(move || {
            let mut buf = [0u8; 4096];
            let mut parser = Parser::new();

            while running_clone.load(Ordering::SeqCst) {
                // Check for pending resize
                if pending_resize_clone.swap(false, Ordering::SeqCst) {
                    let new_cols = cols_clone.load(Ordering::SeqCst) as usize;
                    let new_rows = rows_clone.load(Ordering::SeqCst) as usize;
                    screen_clone.lock().resize(new_cols, new_rows);
                }

                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        let mut screen = screen_clone.lock();
                        for byte in &buf[..n] {
                            parser.advance(&mut *screen, *byte);
                        }
                    }
                    Err(e) => {
                        if e.kind() != std::io::ErrorKind::WouldBlock {
                            error!("Error reading from PTY: {}", e);
                            break;
                        }
                        thread::sleep(Duration::from_millis(5));
                    }
                }
            }
            debug!("Local terminal reader thread exiting");
        });

        Ok(Self {
            pty_pair,
            writer: Arc::new(Mutex::new(writer)),
            screen,
            scroll_offset: 0,
            running,
            cols: cols_atomic,
            rows: rows_atomic,
            pending_resize,
        })
    }

    pub fn write(&mut self, data: &[u8]) -> Result<()> {
        let mut writer = self.writer.lock();
        writer.write_all(data).context("Failed to write to PTY")?;
        writer.flush().context("Failed to flush PTY")?;
        Ok(())
    }

    pub fn send_key(&mut self, key: &str) -> Result<()> {
        self.write(key.as_bytes())
    }

    pub fn get_visible_lines(&self, height: usize) -> Vec<String> {
        let screen = self.screen.lock();
        if self.scroll_offset == 0 {
            let lines = screen.get_lines();
            if lines.len() > height {
                lines[lines.len() - height..].to_vec()
            } else {
                lines
            }
        } else {
            screen.get_lines_with_scrollback(height, self.scroll_offset)
        }
    }

    pub fn scroll_up(&mut self, lines: usize) {
        let total = self.screen.lock().scrollback.len();
        self.scroll_offset = (self.scroll_offset + lines).min(total);
    }

    pub fn scroll_down(&mut self, lines: usize) {
        self.scroll_offset = self.scroll_offset.saturating_sub(lines);
    }

    pub fn scroll_to_bottom(&mut self) {
        self.scroll_offset = 0;
    }

    pub fn resize(&mut self, cols: u16, rows: u16) -> Result<()> {
        let old_cols = self.cols.load(Ordering::SeqCst);
        let old_rows = self.rows.load(Ordering::SeqCst);
        
        if cols == old_cols && rows == old_rows {
            return Ok(());
        }
        
        self.cols.store(cols, Ordering::SeqCst);
        self.rows.store(rows, Ordering::SeqCst);
        self.pending_resize.store(true, Ordering::SeqCst);
        
        self.pty_pair
            .master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("Failed to resize PTY")?;
        Ok(())
    }

    #[allow(dead_code)]
    pub fn size(&self) -> (u16, u16) {
        (self.cols.load(Ordering::SeqCst), self.rows.load(Ordering::SeqCst))
    }
}

impl Drop for LocalTerminal {
    fn drop(&mut self) {
        self.running.store(false, Ordering::SeqCst);
    }
}

pub struct RemoteTerminal {
    channel: Channel,
    screen: Arc<Mutex<TerminalScreen>>,
    scroll_offset: usize,
    #[allow(dead_code)]
    running: Arc<AtomicBool>,
    cols: u16,
    rows: u16,
}

impl RemoteTerminal {
    pub fn new(session_info: &SftpSessionInfo) -> Result<Self> {
        Self::new_with_size(session_info, DEFAULT_COLS, DEFAULT_ROWS)
    }

    pub fn new_with_size(session_info: &SftpSessionInfo, cols: u16, rows: u16) -> Result<Self> {
        info!(
            "Creating remote terminal for {}@{} ({}x{})",
            session_info.username, session_info.host, cols, rows
        );

        let addr = format!("{}:{}", session_info.host, session_info.port);
        let tcp = TcpStream::connect(&addr)
            .with_context(|| format!("Failed to connect to {}", addr))?;

        let tcp_clone = tcp.try_clone()?;

        let mut session = Session::new()?;
        session.set_tcp_stream(tcp);
        session.handshake()?;

        if session.userauth_agent(&session_info.username).is_ok() && session.authenticated() {
        } else if let Some(ref password) = session_info.password {
            session.userauth_password(&session_info.username, password)?;
        } else if let Some(ref key_path) = session_info.key_path {
            let key = PathBuf::from(key_path);
            session.userauth_pubkey_file(&session_info.username, None, &key, None)?;
        } else if let Some(home) = dirs::home_dir() {
            let ssh_dir = home.join(".ssh");
            let mut authenticated = false;
            for key_name in &["id_ed25519", "id_rsa", "id_ecdsa"] {
                let key_path = ssh_dir.join(key_name);
                if key_path.exists() {
                    if session
                        .userauth_pubkey_file(&session_info.username, None, &key_path, None)
                        .is_ok()
                    {
                        authenticated = true;
                        break;
                    }
                }
            }
            if !authenticated {
                anyhow::bail!("Authentication failed");
            }
        }

        if !session.authenticated() {
            anyhow::bail!("Authentication failed");
        }

        let mut channel = session.channel_session()?;
        channel.request_pty(
            "xterm-256color",
            None,
            Some((cols as u32, rows as u32, 0, 0)),
        )?;
        channel.shell()?;
        
        tcp_clone.set_nonblocking(true)?;
        session.set_blocking(false);

        let screen = Arc::new(Mutex::new(TerminalScreen::new(cols as usize, rows as usize)));
        let running = Arc::new(AtomicBool::new(true));

        Ok(Self {
            channel,
            screen,
            scroll_offset: 0,
            running,
            cols,
            rows,
        })
    }

    pub fn poll_read(&mut self) {
        let mut buf = [0u8; 8192];
        let mut parser = Parser::new();
        let mut screen = self.screen.lock();

        for _ in 0..3 {
            match self.channel.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    for byte in &buf[..n] {
                        parser.advance(&mut *screen, *byte);
                    }
                }
                Err(_) => break,
            }
        }
    }

    pub fn write(&mut self, data: &[u8]) -> Result<()> {
        self.poll_read();
        
        for attempt in 0..5 {
            match self.channel.write(data) {
                Ok(_) => {
                    let _ = self.channel.flush();
                    return Ok(());
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    if attempt < 4 {
                        self.poll_read();
                        std::thread::sleep(Duration::from_millis(1));
                    } else {
                        return Err(anyhow::anyhow!("Channel busy, try again"));
                    }
                }
                Err(e) => return Err(anyhow::anyhow!("Failed to write: {}", e)),
            }
        }
        Ok(())
    }

    pub fn send_key(&mut self, key: &str) -> Result<()> {
        self.write(key.as_bytes())
    }

    pub fn get_visible_lines(&self, height: usize) -> Vec<String> {
        let screen = self.screen.lock();
        if self.scroll_offset == 0 {
            let lines = screen.get_lines();
            if lines.len() > height {
                lines[lines.len() - height..].to_vec()
            } else {
                lines
            }
        } else {
            screen.get_lines_with_scrollback(height, self.scroll_offset)
        }
    }

    pub fn scroll_up(&mut self, lines: usize) {
        let total = self.screen.lock().scrollback.len();
        self.scroll_offset = (self.scroll_offset + lines).min(total);
    }

    pub fn scroll_down(&mut self, lines: usize) {
        self.scroll_offset = self.scroll_offset.saturating_sub(lines);
    }

    pub fn scroll_to_bottom(&mut self) {
        self.scroll_offset = 0;
    }

    pub fn resize(&mut self, cols: u16, rows: u16) -> Result<()> {
        if cols == self.cols && rows == self.rows {
            return Ok(());
        }
        
        self.cols = cols;
        self.rows = rows;
        self.screen.lock().resize(cols as usize, rows as usize);
        self.channel.request_pty_size(cols as u32, rows as u32, Some(0), Some(0))?;
        Ok(())
    }

    #[allow(dead_code)]
    pub fn size(&self) -> (u16, u16) {
        (self.cols, self.rows)
    }

    #[allow(dead_code)]
    pub fn is_eof(&self) -> bool {
        self.channel.eof()
    }
}

impl Drop for RemoteTerminal {
    fn drop(&mut self) {
        self.running.store(false, Ordering::SeqCst);
        let _ = self.channel.close();
    }
}
