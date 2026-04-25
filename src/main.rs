mod app;
mod db;
mod editor;
mod ssh;
mod terminal;
mod transfer;
mod ui;

use anyhow::Result;
use app::{
    App, AppMode, ConnectionDialog, FilterMode, FocusPanel, TerminalFocus, SETTINGS_ROW_EDITOR,
};
use crossterm::{
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
        KeyModifiers, MouseButton, MouseEventKind,
    },
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use log::{error, info};
use ratatui::{backend::CrosstermBackend, layout::Rect, Terminal};
use simplelog::{Config, LevelFilter, WriteLogger};
use std::fs::File;
use std::io::{self, stdout};
use std::time::Duration;

fn init_logging() -> Result<()> {
    let log_path = dirs::data_local_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join("badassh");

    std::fs::create_dir_all(&log_path)?;

    let log_file = log_path.join("badassh.log");
    let file = File::create(&log_file)?;

    WriteLogger::init(LevelFilter::Debug, Config::default(), file)?;

    info!("=== badassh started ===");
    info!("Log file: {:?}", log_file);

    Ok(())
}

fn main() -> Result<()> {
    if let Err(e) = init_logging() {
        eprintln!("Warning: Could not initialize logging: {}", e);
    }

    enable_raw_mode()?;
    let mut stdout = stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;

    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut app = App::new()?;
    info!("Application initialized");

    let result = run_app(&mut terminal, &mut app);

    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    terminal.show_cursor()?;

    if let Err(e) = result {
        error!("Application error: {}", e);
        eprintln!("Error: {e}");
    }

    info!("=== badassh exited ===");
    Ok(())
}

fn run_app(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>, app: &mut App) -> Result<()> {
    app.update_local_watcher();

    while app.running {
        terminal.draw(|f| ui::draw(f, app))?;
        app.check_transfers_and_refresh();
        app.check_local_directory_changes();
        app.process_editor_uploads();
        app.poll_remote_terminals();
        app.maybe_sync_explorers_from_terminals();
        app.flush_zip_single_press();

        if event::poll(Duration::from_millis(16))? {
            match event::read()? {
                Event::Key(key) => {
                    if key.kind == KeyEventKind::Press {
                        handle_key_event(app, key);
                    }
                }
                Event::Mouse(mouse) => {
                    if let MouseEventKind::Down(MouseButton::Left) = mouse.kind {
                        let size = terminal.size()?;
                        let rect = Rect::new(0, 0, size.width, size.height);
                        handle_mouse_click(app, mouse.column, mouse.row, rect);
                    }
                }
                Event::Resize(_, _) => {}
                _ => {}
            }
        }
    }

    Ok(())
}

fn handle_key_event(app: &mut App, key: KeyEvent) {
    if app.terminal_focus != TerminalFocus::None {
        handle_terminal_keys(app, key);
        return;
    }

    match app.mode {
        AppMode::Normal => handle_normal_keys(app, key),
        AppMode::MenuFocused => handle_menu_focused_keys(app, key),
        AppMode::MenuOpen => handle_menu_keys(app, key),
        AppMode::ConnectionDialog => handle_dialog_keys(app, key),
        AppMode::ConnectionList => handle_connection_list_keys(app, key),
        AppMode::Connected => handle_connected_keys(app, key),
        AppMode::DirectoryInput => handle_directory_input_keys(app, key),
        AppMode::RenameInput => handle_rename_input_keys(app, key),
        AppMode::DeleteConfirm => handle_delete_confirm_keys(app, key),
        AppMode::ExtractConflictConfirm => handle_extract_conflict_keys(app, key),
        AppMode::Settings => handle_settings_keys(app, key),
        AppMode::KeyboardShortcuts => handle_keyboard_shortcuts_keys(app, key),
    }
}

fn handle_keyboard_shortcuts_keys(app: &mut App, key: KeyEvent) {
    let vh = app.shortcuts_viewport_height.max(1);
    let lines = app.shortcuts_help_line_count.max(1);
    match key.code {
        KeyCode::Esc => {
            app.close_keyboard_shortcuts();
        }
        KeyCode::Up | KeyCode::Char('k') => {
            app.shortcuts_scroll_up();
        }
        KeyCode::Down | KeyCode::Char('j') => {
            app.shortcuts_scroll_down(lines, vh);
        }
        KeyCode::Char('y') | KeyCode::Char('Y')
            if key.modifiers.contains(KeyModifiers::CONTROL) =>
        {
            for _ in 0..vh {
                app.shortcuts_scroll_up();
            }
        }
        KeyCode::Char('v') | KeyCode::Char('V')
            if key.modifiers.contains(KeyModifiers::CONTROL) =>
        {
            for _ in 0..vh {
                app.shortcuts_scroll_down(lines, vh);
            }
        }
        KeyCode::PageUp => {
            for _ in 0..vh {
                app.shortcuts_scroll_up();
            }
        }
        KeyCode::PageDown => {
            for _ in 0..vh {
                app.shortcuts_scroll_down(lines, vh);
            }
        }
        KeyCode::Home => {
            app.shortcuts_scroll_offset = 0;
        }
        KeyCode::End => {
            if lines > vh {
                app.shortcuts_scroll_offset = lines - vh;
            }
        }
        _ => {}
    }
}

fn handle_terminal_keys(app: &mut App, key: KeyEvent) {
    let page = terminal_viewport_lines(app);
    match key.code {
        KeyCode::Esc => {
            match app.terminal_focus {
                TerminalFocus::LocalTerminal => {
                    app.local_terminal_visible = false;
                    app.local_terminal = None;
                }
                TerminalFocus::RemoteTerminal => {
                    if let Some(tab) = app.tabs.get_mut(app.active_tab) {
                        tab.remote_terminal_visible = false;
                        tab.remote_terminal = None;
                    }
                }
                TerminalFocus::None => {}
            }
            app.terminal_focus = TerminalFocus::None;
        }
        KeyCode::Char(c)
            if key.modifiers.contains(KeyModifiers::CONTROL) && matches!(c, 'y' | 'Y') =>
        {
            scroll_terminal(app, true, page);
        }
        KeyCode::Char(c)
            if key.modifiers.contains(KeyModifiers::CONTROL) && matches!(c, 'v' | 'V') =>
        {
            scroll_terminal(app, false, page);
        }
        KeyCode::Char(c) => {
            if key.modifiers.contains(KeyModifiers::CONTROL) {
                let ctrl_char = (c as u8 & 0x1f) as char;
                send_to_terminal(app, &ctrl_char.to_string());
            } else {
                send_to_terminal(app, &c.to_string());
            }
        }
        KeyCode::Enter => {
            send_to_terminal(app, "\r");
        }
        KeyCode::Backspace => {
            send_to_terminal(app, "\x7f");
        }
        KeyCode::Delete => {
            send_to_terminal(app, "\x1b[3~");
        }
        KeyCode::Tab => {
            send_to_terminal(app, "\t");
        }
        KeyCode::Up => {
            if key.modifiers.contains(KeyModifiers::CONTROL) {
                scroll_terminal(app, true, 1);
            } else {
                send_to_terminal(app, "\x1b[A");
            }
        }
        KeyCode::Down => {
            if key.modifiers.contains(KeyModifiers::CONTROL) {
                scroll_terminal(app, false, 1);
            } else {
                send_to_terminal(app, "\x1b[B");
            }
        }
        KeyCode::Right => {
            send_to_terminal(app, "\x1b[C");
        }
        KeyCode::Left => {
            send_to_terminal(app, "\x1b[D");
        }
        KeyCode::Home => {
            send_to_terminal(app, "\x1b[H");
        }
        KeyCode::End => {
            send_to_terminal(app, "\x1b[F");
        }
        KeyCode::PageUp => {
            scroll_terminal(app, true, page);
        }
        KeyCode::PageDown => {
            scroll_terminal(app, false, page);
        }
        _ => {}
    }
}

fn send_to_terminal(app: &mut App, data: &str) {
    match app.terminal_focus {
        TerminalFocus::LocalTerminal => {
            if let Some(term) = &mut app.local_terminal {
                if let Err(e) = term.send_key(data) {
                    error!("Failed to send to local terminal: {}", e);
                    app.error_message = Some(format!("Local terminal error: {}", e));
                }
                term.scroll_to_bottom();
            }
        }
        TerminalFocus::RemoteTerminal => {
            if let Some(tab) = app.tabs.get_mut(app.active_tab) {
                if let Some(term) = &mut tab.remote_terminal {
                    if let Err(e) = term.send_key(data) {
                        error!("Failed to send to remote terminal: {}", e);
                        app.error_message =
                            Some(format!("Remote terminal error for {}: {}", tab.name, e));
                    }
                    term.scroll_to_bottom();
                }
            }
        }
        TerminalFocus::None => {}
    }
}

fn terminal_viewport_lines(app: &App) -> usize {
    match app.terminal_focus {
        TerminalFocus::LocalTerminal => app.visible_local_terminal_rows.max(1),
        TerminalFocus::RemoteTerminal => app.visible_remote_terminal_rows.max(1),
        TerminalFocus::None => 1,
    }
}

fn scroll_terminal(app: &mut App, up: bool, lines: usize) {
    match app.terminal_focus {
        TerminalFocus::LocalTerminal => {
            if let Some(term) = &mut app.local_terminal {
                if up {
                    term.scroll_up(lines);
                } else {
                    term.scroll_down(lines);
                }
            }
        }
        TerminalFocus::RemoteTerminal => {
            if let Some(tab) = app.tabs.get_mut(app.active_tab) {
                if let Some(term) = &mut tab.remote_terminal {
                    if up {
                        term.scroll_up(lines);
                    } else {
                        term.scroll_down(lines);
                    }
                }
            }
        }
        TerminalFocus::None => {}
    }
}

fn handle_normal_keys(app: &mut App, key: KeyEvent) {
    match key.code {
        KeyCode::Tab => {
            if key.modifiers.contains(KeyModifiers::SHIFT) {
                app.cycle_main_focus_backward();
            } else {
                app.cycle_main_focus_forward();
            }
        }
        KeyCode::BackTab => {
            app.cycle_main_focus_backward();
        }
        _ => match app.focus {
            app::FocusPanel::Local => handle_file_browser_keys(app, key),
            app::FocusPanel::Remote => handle_connections_panel_keys(app, key),
            app::FocusPanel::ConnectionTabs => {}
        },
    }
}

fn handle_connections_panel_keys(app: &mut App, key: KeyEvent) {
    let total_items = app.recent_connections.len() + 1;

    match key.code {
        KeyCode::Esc => app.open_file_menu(),
        KeyCode::Up => {
            if app.connection_list_index > 0 {
                app.connection_list_index -= 1;
            }
        }
        KeyCode::Down => {
            if app.connection_list_index < total_items.saturating_sub(1) {
                app.connection_list_index += 1;
            }
        }
        KeyCode::Backspace => {
            if app.connection_list_index < app.recent_connections.len() {
                app.show_delete_saved_connection_confirm();
            }
        }
        KeyCode::Enter => {
            if app.connection_list_index == app.recent_connections.len() {
                app.connection_dialog = ConnectionDialog::new();
                app.mode = AppMode::ConnectionDialog;
            } else if app.connection_list_index < app.recent_connections.len() {
                let saved = app.recent_connections[app.connection_list_index].clone();
                if let Err(e) = app.connect_to_saved(&saved) {
                    error!("Failed to connect to saved host {}: {}", saved.name, e);
                    app.error_message = Some(format!("Connection failed: {}", e));
                }
            }
        }
        _ => {}
    }
}

fn handle_menu_focused_keys(app: &mut App, key: KeyEvent) {
    match key.code {
        KeyCode::Esc => app.close_menu(),
        KeyCode::Char('?') if app.can_open_keyboard_shortcuts() => app.open_keyboard_shortcuts(),
        KeyCode::Left => app.prev_menu_tab(),
        KeyCode::Right => app.next_menu_tab(),
        KeyCode::Tab => {
            if key.modifiers.contains(KeyModifiers::SHIFT) {
                app.cycle_main_focus_backward();
            } else {
                app.cycle_main_focus_forward();
            }
        }
        KeyCode::BackTab => app.cycle_main_focus_backward(),
        KeyCode::Enter | KeyCode::Down => app.open_dropdown(),
        _ => {}
    }
}

fn handle_menu_keys(app: &mut App, key: KeyEvent) {
    match key.code {
        KeyCode::Esc => app.close_menu(),
        KeyCode::Up => app.menu_up(),
        KeyCode::Down => app.menu_down(),
        KeyCode::Left => app.prev_menu_tab(),
        KeyCode::Right => app.next_menu_tab(),
        KeyCode::Tab => {
            if key.modifiers.contains(KeyModifiers::SHIFT) {
                app.prev_menu_tab();
            } else {
                app.next_menu_tab();
            }
        }
        KeyCode::BackTab => app.prev_menu_tab(),
        KeyCode::Enter => app.select_menu_item(),
        _ => {}
    }
}

fn handle_dialog_keys(app: &mut App, key: KeyEvent) {
    match key.code {
        KeyCode::Esc => app.close_dialog(),
        KeyCode::Tab => {
            if key.modifiers.contains(KeyModifiers::SHIFT) {
                app.dialog_prev_field();
            } else {
                app.dialog_next_field();
            }
        }
        KeyCode::Enter => {
            if let Err(e) = app.try_connect() {
                app.connection_dialog.error_message = Some(e.to_string());
            }
        }
        KeyCode::Backspace => app.dialog_backspace(),
        KeyCode::Char(c) => app.dialog_input(c),
        _ => {}
    }
}

fn handle_connection_list_keys(app: &mut App, key: KeyEvent) {
    match key.code {
        KeyCode::Esc => {
            app.mode = if !app.tabs.is_empty() {
                AppMode::Connected
            } else {
                AppMode::Normal
            };
        }
        KeyCode::Up => app.connection_list_up(),
        KeyCode::Down => app.connection_list_down(),
        KeyCode::Enter => {
            if let Err(e) = app.select_connection() {
                app.status_message = Some(format!("Connection failed: {}", e));
            }
        }
        _ => {}
    }
}

fn handle_connected_keys(app: &mut App, key: KeyEvent) {
    if app.focus == FocusPanel::ConnectionTabs {
        match key.code {
            KeyCode::Tab => {
                if key.modifiers.contains(KeyModifiers::SHIFT) {
                    app.cycle_main_focus_backward();
                } else {
                    app.cycle_main_focus_forward();
                }
            }
            KeyCode::BackTab => app.cycle_main_focus_backward(),
            KeyCode::Left => app.tab_bar_highlight_prev(),
            KeyCode::Right => app.tab_bar_highlight_next(),
            KeyCode::Enter => app.activate_highlighted_connection_tab(),
            KeyCode::Esc => app.focus = FocusPanel::Local,
            KeyCode::Char(c)
                if key.modifiers.contains(KeyModifiers::CONTROL) && (c == 'r' || c == 'R') =>
            {
                app.refresh_focused_explorer();
            }
            _ => {}
        }
        return;
    }

    match key.code {
        KeyCode::Tab => {
            if key.modifiers.contains(KeyModifiers::SHIFT) {
                app.cycle_main_focus_backward();
            } else {
                app.cycle_main_focus_forward();
            }
        }
        KeyCode::BackTab => {
            app.cycle_main_focus_backward();
        }
        KeyCode::Char('/') => app.handle_slash_press(),
        _ => match app.focus {
            FocusPanel::Local => handle_local_panel_keys(app, key),
            FocusPanel::Remote => handle_remote_panel_keys(app, key),
            FocusPanel::ConnectionTabs => {}
        },
    }
}

fn handle_directory_input_keys(app: &mut App, key: KeyEvent) {
    match key.code {
        KeyCode::Esc => app.close_directory_input(),
        KeyCode::Enter => app.navigate_to_directory(),
        KeyCode::Tab => app.directory_input_tab_complete(),
        KeyCode::Backspace => app.directory_input_remove_char(),
        KeyCode::Char('/') => app.handle_slash_press(),
        KeyCode::Char(c) => app.directory_input_add_char(c),
        _ => {}
    }
}

fn handle_rename_input_keys(app: &mut App, key: KeyEvent) {
    match key.code {
        KeyCode::Esc => app.close_rename(),
        KeyCode::Enter => app.commit_rename(),
        KeyCode::Backspace => app.rename_input_remove_char(),
        KeyCode::Char(c) => app.rename_input_add_char(c),
        _ => {}
    }
}

fn handle_delete_confirm_keys(app: &mut App, key: KeyEvent) {
    match key.code {
        KeyCode::Esc => {
            app.cancel_delete();
        }
        KeyCode::Left | KeyCode::Right => {
            app.toggle_delete_option();
        }
        KeyCode::Enter => {
            app.confirm_delete();
        }
        KeyCode::Char('y') | KeyCode::Char('Y') => {
            app.delete_confirm_yes = true;
            app.confirm_delete();
        }
        KeyCode::Char('n') | KeyCode::Char('N') => {
            app.cancel_delete();
        }
        _ => {}
    }
}

fn handle_extract_conflict_keys(app: &mut App, key: KeyEvent) {
    match key.code {
        KeyCode::Esc => app.cancel_extract_conflict(),
        KeyCode::Left | KeyCode::Right => app.toggle_extract_conflict_option(),
        KeyCode::Enter => app.confirm_extract_conflict(),
        KeyCode::Char('o') | KeyCode::Char('O') => {
            app.extract_conflict_overwrite = true;
            app.confirm_extract_conflict();
        }
        KeyCode::Char('k') | KeyCode::Char('K') => {
            app.extract_conflict_overwrite = false;
            app.confirm_extract_conflict();
        }
        _ => {}
    }
}

fn handle_settings_keys(app: &mut App, key: KeyEvent) {
    if app.settings_editing_editor {
        match key.code {
            KeyCode::Esc => {
                app.settings_finish_editor_edit();
            }
            KeyCode::Enter => {
                app.settings_finish_editor_edit();
            }
            KeyCode::Backspace => {
                app.settings_editor_remove_char();
            }
            KeyCode::Char(c) => {
                app.settings_editor_add_char(c);
            }
            _ => {}
        }
        return;
    }

    match key.code {
        KeyCode::Esc => {
            app.close_settings();
        }
        KeyCode::Up => {
            app.settings_move_up();
        }
        KeyCode::Down => {
            app.settings_move_down();
        }
        KeyCode::Tab => {
            if app.settings_selected_index + 1 < app::SETTINGS_ROW_COUNT {
                app.settings_selected_index += 1;
            }
        }
        KeyCode::BackTab => {
            app.settings_move_up();
        }
        KeyCode::Left => {
            app.settings_adjust_row(false);
        }
        KeyCode::Right => {
            app.settings_adjust_row(true);
        }
        KeyCode::Char(' ') => {
            app.settings_adjust_row(true);
        }
        KeyCode::Enter => {
            if app.settings_selected_index == SETTINGS_ROW_EDITOR {
                app.settings_begin_editor_edit();
            } else {
                app.settings_adjust_row(true);
            }
        }
        _ => {}
    }
}

fn handle_local_panel_keys(app: &mut App, key: KeyEvent) {
    let page_size = app.visible_file_rows.max(1);
    let is_filtering = app.local.browser.is_filtering();

    if is_filtering {
        match key.code {
            KeyCode::Esc => {
                app.local.browser.clear_filter();
            }
            KeyCode::Enter => {
                if let Err(e) = app.enter_selected() {
                    app.error_message = Some(e.to_string());
                }
                app.local.browser.clear_filter();
            }
            KeyCode::Backspace => {
                app.local.browser.remove_filter_char();
            }
            KeyCode::Up => {
                app.local.browser.move_up();
            }
            KeyCode::Down => {
                app.local.browser.move_down();
            }
            KeyCode::Char(c)
                if key.modifiers.contains(KeyModifiers::CONTROL) && (c == 'r' || c == 'R') =>
            {
                app.refresh_focused_explorer();
            }
            KeyCode::Char(c) => {
                app.local.browser.add_filter_char(c);
            }
            _ => {}
        }
        return;
    }

    match key.code {
        KeyCode::Esc => {
            if !app.try_clear_file_panel_selection() {
                app.open_file_menu();
            }
        }
        KeyCode::Char('?') if app.can_open_keyboard_shortcuts() => app.open_keyboard_shortcuts(),
        KeyCode::Char(c)
            if key.modifiers.contains(KeyModifiers::CONTROL) && (c == 'r' || c == 'R') =>
        {
            app.refresh_focused_explorer();
        }
        KeyCode::Char('r') | KeyCode::Char('R')
            if !key.modifiers.contains(KeyModifiers::CONTROL) =>
        {
            app.try_begin_rename();
        }
        KeyCode::Char(':') => app.local.browser.start_filter(FilterMode::Normal),
        KeyCode::Char(';') => app.local.browser.start_filter(FilterMode::Regex),
        KeyCode::Up => {
            if key.modifiers.contains(KeyModifiers::SHIFT) {
                app.local.browser.move_up_shift();
            } else {
                app.local.browser.move_up();
            }
        }
        KeyCode::Down => {
            if key.modifiers.contains(KeyModifiers::SHIFT) {
                app.local.browser.move_down_shift();
            } else {
                app.local.browser.move_down();
            }
        }
        KeyCode::Char('y') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.local.browser.page_up(page_size);
        }
        KeyCode::Char('v') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.local.browser.page_down(page_size);
        }
        KeyCode::Enter => {
            if let Err(e) = app.enter_selected() {
                app.error_message = Some(e.to_string());
            }
        }
        KeyCode::Char(' ') => app.local.browser.toggle_select_current(),
        KeyCode::Char('e') | KeyCode::Char('E') => app.extract_selected(),
        KeyCode::Char('u') | KeyCode::Char('U') => app.upload_selected(),
        KeyCode::Char('z') | KeyCode::Char('Z') => app.handle_zip_press(),
        KeyCode::Char('x') | KeyCode::Char('X') => app.show_delete_confirm(),
        KeyCode::Char('`') => {
            app.toggle_local_terminal();
            if app.local_terminal_visible {
                app.terminal_focus = TerminalFocus::LocalTerminal;
            }
        }
        _ => {}
    }
}

fn handle_remote_panel_keys(app: &mut App, key: KeyEvent) {
    let page_size = app.visible_file_rows.max(1);
    let is_filtering = app
        .current_tab()
        .map(|t| t.browser.is_filtering())
        .unwrap_or(false);

    if is_filtering {
        match key.code {
            KeyCode::Esc => {
                if let Some(tab) = app.current_tab_mut() {
                    tab.browser.clear_filter();
                }
            }
            KeyCode::Enter => {
                if let Err(e) = app.enter_selected() {
                    app.error_message = Some(e.to_string());
                }
                if let Some(tab) = app.current_tab_mut() {
                    tab.browser.clear_filter();
                }
            }
            KeyCode::Backspace => {
                if let Some(tab) = app.current_tab_mut() {
                    tab.browser.remove_filter_char();
                }
            }
            KeyCode::Up => {
                if let Some(tab) = app.current_tab_mut() {
                    tab.browser.move_up();
                }
            }
            KeyCode::Down => {
                if let Some(tab) = app.current_tab_mut() {
                    tab.browser.move_down();
                }
            }
            KeyCode::Char(c)
                if key.modifiers.contains(KeyModifiers::CONTROL) && (c == 'r' || c == 'R') =>
            {
                app.refresh_focused_explorer();
            }
            KeyCode::Char(c) => {
                if let Some(tab) = app.current_tab_mut() {
                    tab.browser.add_filter_char(c);
                }
            }
            _ => {}
        }
        return;
    }

    match key.code {
        KeyCode::Esc => {
            if !app.try_clear_file_panel_selection() {
                app.open_file_menu();
            }
        }
        KeyCode::Char('?') if app.can_open_keyboard_shortcuts() => app.open_keyboard_shortcuts(),
        KeyCode::Char(c)
            if key.modifiers.contains(KeyModifiers::CONTROL) && (c == 'r' || c == 'R') =>
        {
            app.refresh_focused_explorer();
        }
        KeyCode::Char('r') | KeyCode::Char('R')
            if !key.modifiers.contains(KeyModifiers::CONTROL) =>
        {
            app.try_begin_rename();
        }
        KeyCode::Char(':') => {
            if let Some(tab) = app.current_tab_mut() {
                tab.browser.start_filter(FilterMode::Normal);
            }
        }
        KeyCode::Char(';') => {
            if let Some(tab) = app.current_tab_mut() {
                tab.browser.start_filter(FilterMode::Regex);
            }
        }
        KeyCode::Up => {
            if let Some(tab) = app.current_tab_mut() {
                if key.modifiers.contains(KeyModifiers::SHIFT) {
                    tab.browser.move_up_shift();
                } else {
                    tab.browser.move_up();
                }
            }
        }
        KeyCode::Down => {
            if let Some(tab) = app.current_tab_mut() {
                if key.modifiers.contains(KeyModifiers::SHIFT) {
                    tab.browser.move_down_shift();
                } else {
                    tab.browser.move_down();
                }
            }
        }
        KeyCode::Char('y') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            if let Some(tab) = app.current_tab_mut() {
                tab.browser.page_up(page_size);
            }
        }
        KeyCode::Char('v') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            if let Some(tab) = app.current_tab_mut() {
                tab.browser.page_down(page_size);
            }
        }
        KeyCode::Enter => {
            if let Err(e) = app.enter_selected() {
                app.error_message = Some(e.to_string());
            }
        }
        KeyCode::Char(' ') => {
            if let Some(tab) = app.current_tab_mut() {
                tab.browser.toggle_select_current();
            }
        }
        KeyCode::Char('d') | KeyCode::Char('D') => app.download_selected(),
        KeyCode::Char('e') | KeyCode::Char('E') => app.extract_selected(),
        KeyCode::Char('z') | KeyCode::Char('Z') => app.handle_zip_press(),
        KeyCode::Char('x') | KeyCode::Char('X') => app.show_delete_confirm(),
        KeyCode::Char('`') => {
            app.toggle_remote_terminal();
            if app.is_remote_terminal_visible() {
                app.terminal_focus = TerminalFocus::RemoteTerminal;
            }
        }
        _ => {}
    }
}

fn handle_file_browser_keys(app: &mut App, key: KeyEvent) {
    let page_size = app.visible_file_rows.max(1);
    let is_filtering = app.local.browser.is_filtering();

    if is_filtering {
        match key.code {
            KeyCode::Esc => {
                app.local.browser.clear_filter();
            }
            KeyCode::Enter => {
                let _ = app.enter_selected();
                app.local.browser.clear_filter();
            }
            KeyCode::Backspace => {
                app.local.browser.remove_filter_char();
            }
            KeyCode::Up => {
                app.local.browser.move_up();
            }
            KeyCode::Down => {
                app.local.browser.move_down();
            }
            KeyCode::Char(c)
                if key.modifiers.contains(KeyModifiers::CONTROL) && (c == 'r' || c == 'R') =>
            {
                app.refresh_focused_explorer();
            }
            KeyCode::Char(c) => {
                app.local.browser.add_filter_char(c);
            }
            _ => {}
        }
        return;
    }

    match key.code {
        KeyCode::Esc => {
            if !app.try_clear_file_panel_selection() {
                app.open_file_menu();
            }
        }
        KeyCode::Char('?') if app.can_open_keyboard_shortcuts() => app.open_keyboard_shortcuts(),
        KeyCode::Char(c)
            if key.modifiers.contains(KeyModifiers::CONTROL) && (c == 'r' || c == 'R') =>
        {
            app.refresh_focused_explorer();
        }
        KeyCode::Char('r') | KeyCode::Char('R')
            if !key.modifiers.contains(KeyModifiers::CONTROL) =>
        {
            app.try_begin_rename();
        }
        KeyCode::Char('/') => app.handle_slash_press(),
        KeyCode::Char(':') => app.local.browser.start_filter(FilterMode::Normal),
        KeyCode::Char(';') => app.local.browser.start_filter(FilterMode::Regex),
        KeyCode::Up => {
            if key.modifiers.contains(KeyModifiers::SHIFT) {
                app.local.browser.move_up_shift();
            } else {
                app.local.browser.move_up();
            }
        }
        KeyCode::Down => {
            if key.modifiers.contains(KeyModifiers::SHIFT) {
                app.local.browser.move_down_shift();
            } else {
                app.local.browser.move_down();
            }
        }
        KeyCode::Char('y') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.local.browser.page_up(page_size);
        }
        KeyCode::Char('v') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.local.browser.page_down(page_size);
        }
        KeyCode::Enter => {
            let _ = app.enter_selected();
        }
        KeyCode::Char(' ') => app.local.browser.toggle_select_current(),
        KeyCode::Char('e') | KeyCode::Char('E') => app.extract_selected(),
        KeyCode::Char('z') | KeyCode::Char('Z') => app.handle_zip_press(),
        KeyCode::Char('x') | KeyCode::Char('X') => app.show_delete_confirm(),
        KeyCode::Char('`') => {
            app.toggle_local_terminal();
            if app.local_terminal_visible {
                app.terminal_focus = TerminalFocus::LocalTerminal;
            }
        }
        _ => {}
    }
}

fn handle_mouse_click(app: &mut App, x: u16, y: u16, area: Rect) {
    if y == 0 {
        if x < 7 {
            if app.mode == AppMode::MenuOpen && app.active_menu_tab == app::MenuTab::File {
                app.close_menu();
            } else {
                app.active_menu_tab = app::MenuTab::File;
                app.mode = AppMode::MenuOpen;
            }
            return;
        }
        if x < 18 {
            if app.mode == AppMode::MenuOpen && app.active_menu_tab == app::MenuTab::Connect {
                app.close_menu();
            } else {
                app.active_menu_tab = app::MenuTab::Connect;
                app.mode = AppMode::MenuOpen;
            }
            return;
        }
        if app.mode == AppMode::MenuOpen && app.active_menu_tab == app::MenuTab::Help {
            app.close_menu();
        } else {
            app.active_menu_tab = app::MenuTab::Help;
            app.mode = AppMode::MenuOpen;
        }
        return;
    }

    if app.mode == AppMode::MenuOpen {
        match app.active_menu_tab {
            app::MenuTab::File => {
                if x >= 1 && x < 24 && y >= 2 && y <= 3 {
                    let item_index = (y - 2) as usize;
                    if item_index < 2 {
                        app.file_menu_index = item_index;
                        app.select_menu_item();
                        return;
                    }
                }
            }
            app::MenuTab::Connect => {
                if x >= 8 && x < 30 && y >= 2 && y <= 4 {
                    let item_index = (y - 2) as usize;
                    if item_index < 3 {
                        app.connect_menu_index = item_index;
                        app.select_menu_item();
                        return;
                    }
                }
            }
            app::MenuTab::Help => {
                if x >= 18 && x < 62 && y >= 2 && y <= 2 {
                    app.select_menu_item();
                    return;
                }
            }
        }
        app.close_menu();
        return;
    }

    let tab_bar_y = area.height.saturating_sub(2);
    if y == tab_bar_y && !app.tabs.is_empty() {
        let mut tab_x = 1u16;
        for (i, tab) in app.tabs.iter().enumerate() {
            let tab_width = tab.name.len() as u16 + 2;
            if x >= tab_x && x < tab_x + tab_width {
                app.active_tab = i;
                app.tab_bar_highlight = i;
                app.focus = FocusPanel::Local;
                app.mode = AppMode::Connected;
                return;
            }
            tab_x += tab_width + 1;
        }
    }

    if matches!(
        app.mode,
        AppMode::Connected | AppMode::DirectoryInput | AppMode::RenameInput
    ) {
        let main_area_y_start = 1;
        let main_area_y_end = area.height.saturating_sub(3);

        if y > main_area_y_start && y < main_area_y_end {
            let mid_x = area.width / 2;
            if x < mid_x {
                app.focus = FocusPanel::Local;
            } else {
                app.focus = FocusPanel::Remote;
            }

            if app.mode == AppMode::DirectoryInput {
                app.close_directory_input();
            }
            if app.mode == AppMode::RenameInput {
                app.close_rename();
            }
        }
    }
}
