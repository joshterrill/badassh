use crate::app::{
    App, AppMode, DialogField, ExplorerColumns, FileBrowser, FilterMode, FocusPanel, MenuTab,
    TerminalFocus, SETTINGS_ROW_COUNT, SETTINGS_ROW_EDITOR,
};
use ratatui::{
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState, Wrap},
    Frame,
};

const MENU_BG: Color = Color::Rgb(40, 44, 52);
const MENU_SELECTED: Color = Color::Rgb(97, 175, 239);
const ACCENT: Color = Color::Rgb(152, 195, 121);
const BORDER: Color = Color::Rgb(92, 99, 112);
const TEXT: Color = Color::Rgb(171, 178, 191);
const TEXT_DIM: Color = Color::Rgb(92, 99, 112);
const ERROR: Color = Color::Rgb(224, 108, 117);
const PANEL_BG: Color = Color::Rgb(33, 37, 43);
const DIR_COLOR: Color = Color::Rgb(97, 175, 239);
const FILE_COLOR: Color = Color::Rgb(171, 178, 191);
const TAB_ACTIVE: Color = Color::Rgb(152, 195, 121);
const TAB_INACTIVE: Color = Color::Rgb(92, 99, 112);
const SELECTED_BG: Color = Color::Rgb(60, 70, 90);
const INPUT_BG: Color = Color::Rgb(50, 55, 65);

pub fn draw(frame: &mut Frame, app: &mut App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(0),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(frame.area());

    draw_menu_bar(frame, app, chunks[0]);

    let is_connected = !app.tabs.is_empty();
    match app.mode {
        AppMode::Connected | AppMode::DirectoryInput | AppMode::DeleteConfirm => {
            draw_connected_view(frame, app, chunks[1]);
        }
        AppMode::MenuFocused | AppMode::MenuOpen | AppMode::Settings |
        AppMode::KeyboardShortcuts | AppMode::ConnectionDialog |
        AppMode::ConnectionList => {
            if is_connected {
                draw_connected_view(frame, app, chunks[1]);
            } else {
                draw_normal_view(frame, app, chunks[1]);
            }
        }
        _ => draw_normal_view(frame, app, chunks[1]),
    }

    draw_tab_bar(frame, app, chunks[2]);
    draw_status_bar(frame, app, chunks[3]);

    if app.mode == AppMode::MenuOpen {
        draw_dropdown_menu(frame, app);
    }

    if app.mode == AppMode::ConnectionDialog {
        draw_connection_dialog(frame, app);
    }

    if app.mode == AppMode::ConnectionList {
        draw_connection_list(frame, app);
    }

    if app.mode == AppMode::DeleteConfirm {
        draw_delete_confirm(frame, app, chunks[1]);
    }
    
    if app.mode == AppMode::Settings {
        draw_settings(frame, app);
    }
    
    if app.mode == AppMode::KeyboardShortcuts {
        draw_keyboard_shortcuts(frame, app);
    }
}

fn draw_menu_bar(frame: &mut Frame, app: &App, area: Rect) {
    let menu_items = vec![
        ("File", MenuTab::File),
        ("Connect", MenuTab::Connect),
        ("Help", MenuTab::Help),
    ];

    let is_menu_active = app.mode == AppMode::MenuFocused || app.mode == AppMode::MenuOpen;

    let spans: Vec<Span> = menu_items
        .iter()
        .flat_map(|(name, tab)| {
            let style = if is_menu_active && app.active_menu_tab == *tab {
                Style::default()
                    .fg(Color::Black)
                    .bg(MENU_SELECTED)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(TEXT)
            };

            vec![
                Span::styled(format!(" {} ", name), style),
                Span::styled(" ", Style::default().bg(MENU_BG)),
            ]
        })
        .collect();

    let menu_line = Line::from(spans);
    let menu = Paragraph::new(menu_line).style(Style::default().bg(MENU_BG));
    frame.render_widget(menu, area);
}

fn draw_tab_bar(frame: &mut Frame, app: &App, area: Rect) {
    let strip_focused = app.focus == FocusPanel::ConnectionTabs;
    let mut spans: Vec<Span> = vec![Span::styled(" ", Style::default().bg(MENU_BG))];

    if app.tabs.is_empty() {
        spans.push(Span::styled(
            "No connections",
            Style::default().fg(TEXT_DIM).bg(MENU_BG),
        ));
    } else {
        for (i, tab) in app.tabs.iter().enumerate() {
            let is_keyboard_target = strip_focused && i == app.tab_bar_highlight;
            let is_active_session = i == app.active_tab;
            let style = if is_keyboard_target {
                Style::default()
                    .fg(Color::Black)
                    .bg(MENU_SELECTED)
                    .add_modifier(Modifier::BOLD)
            } else if is_active_session {
                Style::default()
                    .fg(Color::Black)
                    .bg(TAB_ACTIVE)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(TAB_INACTIVE).bg(MENU_BG)
            };

            spans.push(Span::styled(format!(" {} ", tab.name), style));
            spans.push(Span::styled(" ", Style::default().bg(MENU_BG)));
        }
    }

    let tab_line = Line::from(spans);
    let tab_bar = Paragraph::new(tab_line).style(Style::default().bg(MENU_BG));
    frame.render_widget(tab_bar, area);
}

fn draw_dropdown_menu(frame: &mut Frame, app: &App) {
    let (items, x_offset, selected_idx): (Vec<String>, u16, usize) = match app.active_menu_tab {
        MenuTab::File => (
            vec!["Settings".to_string(), "Exit".to_string()],
            1,
            app.file_menu_index,
        ),
        MenuTab::Connect => (
            vec!["New Connection".to_string(), "Recent Connections".to_string(), "Show All Connections".to_string()],
            8,
            app.connect_menu_index,
        ),
        MenuTab::Help => (
            vec!["Keyboard Shortcuts".to_string()],
            18,
            app.help_menu_index,
        ),
    };

    let width = items.iter().map(|s| s.len()).max().unwrap_or(10) as u16 + 4;
    let height = items.len() as u16 + 2;

    let area = Rect::new(x_offset, 1, width, height);

    frame.render_widget(Clear, area);

    let list_items: Vec<ListItem> = items
        .iter()
        .enumerate()
        .map(|(i, item)| {
            let style = if i == selected_idx {
                Style::default()
                    .fg(Color::Black)
                    .bg(MENU_SELECTED)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(TEXT)
            };
            ListItem::new(Span::styled(format!(" {} ", item), style))
        })
        .collect();

    let list = List::new(list_items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(BORDER))
                .style(Style::default().bg(PANEL_BG)),
        );

    frame.render_widget(list, area);
}

fn draw_normal_view(frame: &mut Frame, app: &mut App, area: Rect) {
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(50),
            Constraint::Percentage(50),
        ])
        .split(area);
    
    let columns = app.preferences.explorer_columns;
    let in_dir_input = app.mode == AppMode::DirectoryInput && app.focus == FocusPanel::Local;
    let dir_input = if in_dir_input { Some(&app.directory_input) } else { None };
    let local_focused = app.focus == FocusPanel::Local && app.mode != AppMode::MenuOpen && app.mode != AppMode::MenuFocused;
    let terminal_focused = app.terminal_focus == TerminalFocus::LocalTerminal;
    
    if app.local_terminal_visible {
        let local_chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Percentage(67),
                Constraint::Percentage(33),
            ])
            .split(chunks[0]);
        
        draw_file_panel(
            frame, 
            &mut app.local.browser, 
            "Local", 
            local_focused && !in_dir_input && !terminal_focused, 
            local_chunks[0],
            &mut app.visible_file_rows,
            &columns,
            dir_input,
        );
        
        draw_terminal_panel(
            frame,
            app.local_terminal.as_mut(),
            "Local Terminal",
            terminal_focused,
            local_chunks[1],
        );
    } else {
        draw_file_panel(
            frame, 
            &mut app.local.browser, 
            "Local", 
            local_focused && !in_dir_input, 
            chunks[0],
            &mut app.visible_file_rows,
            &columns,
            dir_input,
        );
    }
    
    let remote_focused = app.focus == FocusPanel::Remote && app.mode != AppMode::MenuOpen && app.mode != AppMode::MenuFocused;
    draw_connections_panel(frame, app, chunks[1], remote_focused);
}

fn draw_connections_panel(frame: &mut Frame, app: &App, area: Rect, is_focused: bool) {
    let border_color = if is_focused { ACCENT } else { BORDER };
    
    let block = Block::default()
        .title(Span::styled(
            " Recent Connections ",
            if is_focused {
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(TEXT_DIM).add_modifier(Modifier::BOLD)
            },
        ))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border_color))
        .style(Style::default().bg(PANEL_BG));
    
    let inner = block.inner(area);
    frame.render_widget(block, area);
    
    let connections = &app.recent_connections;
    let _total_items = connections.len() + 1;
    
    let mut items: Vec<ListItem> = connections
        .iter()
        .enumerate()
        .map(|(i, conn)| {
            let is_selected = app.connection_list_index == i && is_focused;
            let text = format!(" {} ({}@{}:{}) ", conn.name, conn.username, conn.host, conn.port);
            
            let style = if is_selected {
                Style::default().bg(MENU_SELECTED).fg(Color::Black).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(TEXT)
            };
            
            ListItem::new(Span::styled(text, style))
        })
        .collect();
    
    if !connections.is_empty() {
        items.push(ListItem::new(Span::styled(
            " ─────────────────────────── ",
            Style::default().fg(BORDER),
        )));
    }
    
    let new_conn_selected = app.connection_list_index == connections.len() && is_focused;
    let new_conn_style = if new_conn_selected {
        Style::default().bg(MENU_SELECTED).fg(Color::Black).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(ACCENT)
    };
    items.push(ListItem::new(Span::styled(" + New Connection ", new_conn_style)));
    
    if connections.is_empty() {
        let empty_msg = ListItem::new(Span::styled(
            " No recent connections ",
            Style::default().fg(TEXT_DIM),
        ));
        items.insert(0, empty_msg);
    }
    
    let list = List::new(items)
        .style(Style::default().bg(PANEL_BG));
    
    frame.render_widget(list, inner);
    
    if is_focused && inner.height > 2 {
        let hint = Paragraph::new(Span::styled(
            "Enter:Connect Tab:Next area",
            Style::default().fg(TEXT_DIM),
        ))
        .alignment(Alignment::Center);
        let hint_area = Rect::new(inner.x, inner.y + inner.height - 1, inner.width, 1);
        frame.render_widget(hint, hint_area);
    }
}

fn draw_connected_view(frame: &mut Frame, app: &mut App, area: Rect) {
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(50),
            Constraint::Percentage(50),
        ])
        .split(area);

    let in_dir_input = app.mode == AppMode::DirectoryInput;
    let local_terminal_focused = app.terminal_focus == TerminalFocus::LocalTerminal;
    let remote_terminal_focused = app.terminal_focus == TerminalFocus::RemoteTerminal;
    let local_focused = app.focus == FocusPanel::Local && !in_dir_input && !local_terminal_focused;
    let remote_focused = app.focus == FocusPanel::Remote && !in_dir_input && !remote_terminal_focused;
    let columns = app.preferences.explorer_columns;
    
    let local_dir_input = if in_dir_input && app.focus == FocusPanel::Local {
        Some(&app.directory_input)
    } else {
        None
    };
    let remote_dir_input = if in_dir_input && app.focus == FocusPanel::Remote {
        Some(&app.directory_input)
    } else {
        None
    };
    
    if app.local_terminal_visible {
        let local_chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Percentage(67),
                Constraint::Percentage(33),
            ])
            .split(chunks[0]);
        
        draw_file_panel(
            frame, 
            &mut app.local.browser, 
            "Local", 
            local_focused, 
            local_chunks[0],
            &mut app.visible_file_rows,
            &columns,
            local_dir_input,
        );
        
        draw_terminal_panel(
            frame,
            app.local_terminal.as_mut(),
            "Local Terminal",
            local_terminal_focused,
            local_chunks[1],
        );
    } else {
        draw_file_panel(
            frame, 
            &mut app.local.browser, 
            "Local", 
            local_focused, 
            chunks[0],
            &mut app.visible_file_rows,
            &columns,
            local_dir_input,
        );
    }

    let remote_terminal_visible = app.tabs.get(app.active_tab)
        .map(|t| t.remote_terminal_visible)
        .unwrap_or(false);
    
    if remote_terminal_visible {
        let remote_chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Percentage(67),
                Constraint::Percentage(33),
            ])
            .split(chunks[1]);
        
        if let Some(tab) = app.tabs.get_mut(app.active_tab) {
            draw_file_panel(
                frame,
                &mut tab.browser,
                &tab.name.clone(),
                remote_focused,
                remote_chunks[0],
                &mut app.visible_file_rows,
                &columns,
                remote_dir_input,
            );
            
            draw_remote_terminal_panel(
                frame,
                tab.remote_terminal.as_mut(),
                "Remote Terminal",
                remote_terminal_focused,
                remote_chunks[1],
            );
        }
    } else if let Some(tab) = app.tabs.get_mut(app.active_tab) {
        draw_file_panel(
            frame,
            &mut tab.browser,
            &tab.name.clone(),
            remote_focused,
            chunks[1],
            &mut app.visible_file_rows,
            &columns,
            remote_dir_input,
        );
    }
}

fn draw_terminal_panel(
    frame: &mut Frame,
    terminal: Option<&mut crate::terminal::LocalTerminal>,
    title: &str,
    is_focused: bool,
    area: Rect,
) {
    let border_color = if is_focused { ACCENT } else { BORDER };
    
    let block = Block::default()
        .title(Span::styled(
            format!(" {} ", title),
            if is_focused {
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(TEXT_DIM).add_modifier(Modifier::BOLD)
            },
        ))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border_color))
        .style(Style::default().bg(Color::Rgb(25, 28, 33)));
    
    let inner = block.inner(area);
    frame.render_widget(block, area);
    
    if let Some(term) = terminal {
        let _ = term.resize(inner.width, inner.height);
        
        let height = inner.height as usize;
        let lines = term.get_visible_lines(height);
        
        let text_lines: Vec<Line> = lines
            .iter()
            .map(|line| Line::from(Span::styled(line.clone(), Style::default().fg(TEXT))))
            .collect();
        
        let para = Paragraph::new(text_lines)
            .style(Style::default().bg(Color::Rgb(25, 28, 33)));
        
        frame.render_widget(para, inner);
    } else {
        let text = Paragraph::new("Terminal not initialized")
            .style(Style::default().fg(TEXT_DIM))
            .alignment(Alignment::Center);
        frame.render_widget(text, inner);
    }
}

fn draw_remote_terminal_panel(
    frame: &mut Frame,
    terminal: Option<&mut crate::terminal::RemoteTerminal>,
    title: &str,
    is_focused: bool,
    area: Rect,
) {
    let border_color = if is_focused { ACCENT } else { BORDER };
    
    let block = Block::default()
        .title(Span::styled(
            format!(" {} ", title),
            if is_focused {
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(TEXT_DIM).add_modifier(Modifier::BOLD)
            },
        ))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border_color))
        .style(Style::default().bg(Color::Rgb(25, 28, 33)));
    
    let inner = block.inner(area);
    frame.render_widget(block, area);
    
    if let Some(term) = terminal {
        let _ = term.resize(inner.width, inner.height);
        
        let height = inner.height as usize;
        let lines = term.get_visible_lines(height);
        
        let text_lines: Vec<Line> = lines
            .iter()
            .map(|line| Line::from(Span::styled(line.clone(), Style::default().fg(TEXT))))
            .collect();
        
        let para = Paragraph::new(text_lines)
            .style(Style::default().bg(Color::Rgb(25, 28, 33)));
        
        frame.render_widget(para, inner);
    } else {
        let text = Paragraph::new("Terminal not initialized")
            .style(Style::default().fg(TEXT_DIM))
            .alignment(Alignment::Center);
        frame.render_widget(text, inner);
    }
}

fn draw_file_panel(
    frame: &mut Frame,
    browser: &mut FileBrowser,
    title: &str,
    is_focused: bool,
    area: Rect,
    visible_rows: &mut usize,
    columns: &ExplorerColumns,
    directory_input: Option<&String>,
) {
    let in_dir_input = directory_input.is_some();
    let border_color = if in_dir_input { ACCENT } else if is_focused { ACCENT } else { BORDER };

    let full_title = if let Some(dir_input) = directory_input {
        format!(" {} - {}█ ", title, dir_input)
    } else {
        match browser.filter_mode {
            FilterMode::None => format!(" {} - {} ", title, browser.current_dir),
            FilterMode::Normal => format!(" {} - {} [filter: {}] ", title, browser.current_dir, browser.filter),
            FilterMode::Regex => format!(" {} - {} [regex: {}] ", title, browser.current_dir, browser.filter),
        }
    };
    
    let title_style = if in_dir_input {
        Style::default().fg(Color::White).bg(ACCENT).add_modifier(Modifier::BOLD)
    } else if is_focused {
        Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(TEXT_DIM).add_modifier(Modifier::BOLD)
    };
    
    let block = Block::default()
        .title(Span::styled(full_title, title_style))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border_color))
        .style(Style::default().bg(PANEL_BG));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    let file_list_height = inner.height as usize;
    *visible_rows = file_list_height;

    let files_area = Rect::new(inner.x, inner.y, inner.width.saturating_sub(1), inner.height);
    
    let total_width = files_area.width as usize;
    let mut extra_cols_width = 0usize;
    if columns.show_size { extra_cols_width += 10; }
    if columns.show_permissions { extra_cols_width += 12; }
    if columns.show_modified { extra_cols_width += 18; }
    if columns.show_created { extra_cols_width += 18; }
    
    let name_width = total_width.saturating_sub(extra_cols_width).max(20);
    
    let filtered_len = browser.filtered_files().len();
    let selected_in_filtered = browser.selected_index.min(filtered_len.saturating_sub(1));
    
    if selected_in_filtered < browser.scroll_offset {
        browser.scroll_offset = selected_in_filtered;
    } else if file_list_height > 0 && selected_in_filtered >= browser.scroll_offset + file_list_height {
        browser.scroll_offset = selected_in_filtered.saturating_sub(file_list_height) + 1;
    }
    
    let scroll_offset = browser.scroll_offset.min(filtered_len.saturating_sub(1));
    let filtered = browser.filtered_files();
    
    let items: Vec<ListItem> = filtered
        .iter()
        .enumerate()
        .skip(scroll_offset)
        .take(file_list_height)
        .map(|(filtered_idx, (original_idx, file))| {
            let is_cursor = filtered_idx == selected_in_filtered;
            let is_selected = browser.is_selected(*original_idx);
            let name_color = if file.is_dir { DIR_COLOR } else { FILE_COLOR };
            
            let base_style = if is_cursor && is_focused {
                Style::default().bg(MENU_SELECTED).fg(Color::Black).add_modifier(Modifier::BOLD)
            } else if is_selected {
                Style::default().bg(SELECTED_BG).fg(name_color)
            } else {
                Style::default().fg(name_color)
            };
            
            let dim_style = if is_cursor && is_focused {
                Style::default().bg(MENU_SELECTED).fg(Color::Black)
            } else if is_selected {
                Style::default().bg(SELECTED_BG).fg(TEXT_DIM)
            } else {
                Style::default().fg(TEXT_DIM)
            };

            let icon = if file.is_dir { "📁 " } else { "📄 " };
            let marker = if is_selected && !is_cursor { "✓ " } else { "  " };
            
            let name_display = format!("{}{}{}", marker, icon, file.name);
            let name_truncated = if name_display.chars().count() > name_width {
                let mut s: String = name_display.chars().take(name_width.saturating_sub(1)).collect();
                s.push('…');
                s
            } else {
                format!("{:width$}", name_display, width = name_width)
            };
            
            let mut spans = vec![Span::styled(name_truncated, base_style)];
            
            if columns.show_size {
                spans.push(Span::styled(format!("{:>9} ", file.size), dim_style));
            }
            if columns.show_permissions {
                spans.push(Span::styled(format!("{:<11} ", file.permissions), dim_style));
            }
            if columns.show_modified {
                spans.push(Span::styled(format!("{:<17} ", file.modified), dim_style));
            }
            if columns.show_created {
                let created = file.created.as_deref().unwrap_or("");
                spans.push(Span::styled(format!("{:<17} ", created), dim_style));
            }

            ListItem::new(Line::from(spans))
        })
        .collect();

    let list = List::new(items);
    frame.render_widget(list, files_area);

    if filtered_len > file_list_height {
        let scrollbar_area = Rect::new(inner.x + inner.width - 1, inner.y, 1, inner.height);
        let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .begin_symbol(Some("▲"))
            .end_symbol(Some("▼"))
            .track_symbol(Some("│"))
            .thumb_symbol("█");
        
        let mut scrollbar_state = ScrollbarState::new(filtered_len)
            .position(selected_in_filtered);
        
        frame.render_stateful_widget(scrollbar, scrollbar_area, &mut scrollbar_state);
    }
}

fn draw_connection_dialog(frame: &mut Frame, app: &App) {
    let screen = frame.area();
    
    let dialog_width = 70u16.min(screen.width.saturating_sub(2));
    let dialog_height = 25u16.min(screen.height.saturating_sub(2));
    
    let dialog_x = screen.width.saturating_sub(dialog_width) / 2;
    let dialog_y = screen.height.saturating_sub(dialog_height) / 2;
    
    let area = Rect::new(dialog_x, dialog_y, dialog_width, dialog_height);
    frame.render_widget(Clear, area);

    let dialog_block = Block::default()
        .title(Span::styled(
            " New Connection ",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(ACCENT))
        .style(Style::default().bg(PANEL_BG));

    frame.render_widget(dialog_block, area);

    let inner_x = area.x + 2;
    let inner_y = area.y + 1;
    let inner_width = area.width.saturating_sub(4);

    let fields = [
        (DialogField::Name, "Name (optional)"),
        (DialogField::Host, "Host *"),
        (DialogField::Port, "Port"),
        (DialogField::Username, "Username *"),
        (DialogField::Password, "Password"),
        (DialogField::KeyPath, "Key Path"),
    ];

    let field_height = 3u16;
    
    for (i, (field, label)) in fields.iter().enumerate() {
        let field_y = inner_y + (i as u16 * field_height);
        
        if field_y + field_height > area.y + area.height - 1 {
            continue;
        }
        
        let field_area = Rect::new(inner_x, field_y, inner_width, field_height);
        
        let is_active = app.connection_dialog.active_field == *field;
        let value = app.connection_dialog.get_field_value(*field);

        let display_value = if *field == DialogField::Password && !value.is_empty() {
            "*".repeat(value.len())
        } else {
            value.to_string()
        };

        let (border_color, bg_color, title_color) = if is_active {
            (MENU_SELECTED, Color::Rgb(50, 55, 65), MENU_SELECTED)
        } else {
            (BORDER, PANEL_BG, TEXT_DIM)
        };

        let input_block = Block::default()
            .title(Span::styled(
                format!(" {} ", label),
                Style::default().fg(title_color),
            ))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(border_color))
            .style(Style::default().bg(bg_color));

        let display_text = if is_active {
            format!(" {}█", display_value)
        } else if display_value.is_empty() {
            " ".to_string()
        } else {
            format!(" {}", display_value)
        };

        let input = Paragraph::new(display_text)
            .block(input_block)
            .style(Style::default().fg(if is_active { Color::White } else { TEXT }));

        frame.render_widget(input, field_area);
    }

    if let Some(ref error) = app.connection_dialog.error_message {
        let error_y = inner_y + (6 * field_height);
        if error_y < area.y + area.height - 1 {
            let error_area = Rect::new(inner_x, error_y, inner_width, 2);
            
            let error_para = Paragraph::new(Span::styled(
                error.as_str(),
                Style::default().fg(ERROR).add_modifier(Modifier::BOLD),
            ))
            .alignment(Alignment::Center)
            .wrap(Wrap { trim: true });

            frame.render_widget(error_para, error_area);
        }
    }
}

fn draw_connection_list(frame: &mut Frame, app: &App) {
    let screen = frame.area();
    
    let list_width = 50u16.min(screen.width.saturating_sub(4));
    let list_height = 20u16.min(screen.height.saturating_sub(2));
    
    let list_x = (screen.width.saturating_sub(list_width)) / 2;
    let list_y = (screen.height.saturating_sub(list_height)) / 2;
    
    let area = Rect::new(list_x, list_y, list_width, list_height);
    frame.render_widget(Clear, area);

    let title = if app.showing_recent {
        " Recent Connections "
    } else {
        " All Connections "
    };

    let block = Block::default()
        .title(Span::styled(
            title,
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(ACCENT))
        .style(Style::default().bg(PANEL_BG));

    let connections = app.get_current_connections();

    if connections.is_empty() {
        let empty_msg = Paragraph::new(Span::styled(
            "No connections saved yet",
            Style::default().fg(TEXT_DIM).add_modifier(Modifier::ITALIC),
        ))
        .block(block)
        .alignment(Alignment::Center);

        frame.render_widget(empty_msg, area);
    } else {
        let items: Vec<ListItem> = connections
            .iter()
            .enumerate()
            .map(|(i, conn)| {
                let style = if i == app.connection_list_index {
                    Style::default()
                        .fg(Color::Black)
                        .bg(MENU_SELECTED)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(TEXT)
                };

                ListItem::new(Line::from(vec![
                    Span::styled(
                        format!(" {} ", conn.name),
                        style,
                    ),
                    Span::styled(
                        format!(" ({}@{}:{}) ", conn.username, conn.host, conn.port),
                        if i == app.connection_list_index {
                            style
                        } else {
                            Style::default().fg(TEXT_DIM)
                        },
                    ),
                ]))
            })
            .collect();

        let list = List::new(items).block(block);
        frame.render_widget(list, area);
    }
}

fn draw_delete_confirm(frame: &mut Frame, app: &App, main_area: Rect) {
    // Determine which panel to center the dialog in
    let panel_area = if app.tabs.is_empty() {
        main_area
    } else {
        let half_width = main_area.width / 2;
        match app.focus {
            FocusPanel::Local => Rect::new(main_area.x, main_area.y, half_width, main_area.height),
            FocusPanel::Remote => Rect::new(main_area.x + half_width, main_area.y, main_area.width - half_width, main_area.height),
            FocusPanel::ConnectionTabs => main_area,
        }
    };

    let n = app.delete_targets.len();
    if n == 0 {
        return;
    }

    let dialog_width = 52u16.min(panel_area.width.saturating_sub(4));
    let body_lines: u16 = if n == 1 { 1 } else { 2 };
    let detail_h: u16 = if n <= 1 {
        0
    } else if n <= 6 {
        1
    } else {
        2
    };
    // Inner: question + optional name list + gap + button row
    let inner_content_h = body_lines + detail_h + 1 + 1;
    let dialog_height = (2 + inner_content_h)
        .min(panel_area.height.saturating_sub(2))
        .max(6);

    let dialog_x = panel_area.x + (panel_area.width.saturating_sub(dialog_width)) / 2;
    let dialog_y = panel_area.y + (panel_area.height.saturating_sub(dialog_height)) / 2;

    let area = Rect::new(dialog_x, dialog_y, dialog_width, dialog_height);
    frame.render_widget(Clear, area);

    let block = Block::default()
        .title(Span::styled(
            " Confirm Delete ",
            Style::default().fg(ERROR).add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(ERROR))
        .style(Style::default().bg(PANEL_BG));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    let question = if n == 1 {
        format!("Delete \"{}\"?", app.delete_targets[0].0)
    } else {
        format!("Delete {} selected items?", n)
    };
    let question_para = Paragraph::new(Span::styled(question, Style::default().fg(TEXT)))
        .alignment(Alignment::Center)
        .wrap(Wrap { trim: true });

    let question_area = Rect::new(inner.x, inner.y, inner.width, body_lines);
    frame.render_widget(question_para, question_area);

    let mut btn_y = inner.y + body_lines;
    if n > 1 {
        let names: String = if n <= 6 {
            app.delete_targets
                .iter()
                .map(|(name, _)| name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        } else {
            format!(
                "{}, … (+{} more)",
                app.delete_targets[..4]
                    .iter()
                    .map(|(name, _)| name.as_str())
                    .collect::<Vec<_>>()
                    .join(", "),
                n - 4
            )
        };
        let detail_para = Paragraph::new(Span::styled(names, Style::default().fg(TEXT_DIM)))
            .alignment(Alignment::Center)
            .wrap(Wrap { trim: true });
        let detail_area = Rect::new(inner.x, btn_y, inner.width, detail_h);
        frame.render_widget(detail_para, detail_area);
        btn_y += detail_h;
    }
    btn_y += 1;

    let yes_style = if app.delete_confirm_yes {
        Style::default().fg(Color::Black).bg(ERROR).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(TEXT)
    };

    let no_style = if !app.delete_confirm_yes {
        Style::default().fg(Color::Black).bg(ACCENT).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(TEXT)
    };

    let buttons = Line::from(vec![
        Span::styled("  [ Yes ]  ", yes_style),
        Span::raw("    "),
        Span::styled("  [ No ]  ", no_style),
    ]);

    let buttons_para = Paragraph::new(buttons).alignment(Alignment::Center);
    let buttons_area = Rect::new(inner.x, btn_y, inner.width, 1);
    frame.render_widget(buttons_para, buttons_area);
}

fn draw_settings(frame: &mut Frame, app: &App) {
    let screen = frame.area();
    let dialog_width = (screen.width.saturating_sub(4)).min(78);
    let body_lines = SETTINGS_ROW_COUNT as u16;
    let dialog_height = (body_lines + 4).min(screen.height.saturating_sub(4));

    let dialog_x = screen.width.saturating_sub(dialog_width) / 2;
    let dialog_y = screen.height.saturating_sub(dialog_height) / 2;

    let area = Rect::new(dialog_x, dialog_y, dialog_width, dialog_height);
    frame.render_widget(Clear, area);

    let dialog_block = Block::default()
        .title(Span::styled(
            " Settings ",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(ACCENT))
        .style(Style::default().bg(PANEL_BG));

    let inner = dialog_block.inner(area);
    frame.render_widget(dialog_block, area);

    let cols = app.preferences.explorer_columns;
    let p = &app.preferences;

    for i in 0..SETTINGS_ROW_COUNT {
        let is_selected = app.settings_selected_index == i && !app.settings_editing_editor;
        let is_editor_row = i == SETTINGS_ROW_EDITOR;
        let editing_here = is_editor_row && app.settings_editing_editor;

        let line = match i {
            0 => format!(
                " {} File explorer: file size column",
                if cols.show_size { "[✓]" } else { "[ ]" }
            ),
            1 => format!(
                " {} File explorer: permissions column",
                if cols.show_permissions { "[✓]" } else { "[ ]" }
            ),
            2 => format!(
                " {} File explorer: last modified column",
                if cols.show_modified { "[✓]" } else { "[ ]" }
            ),
            3 => format!(
                " {} File explorer: created column",
                if cols.show_created { "[✓]" } else { "[ ]" }
            ),
            4 => {
                let cmd = if editing_here {
                    format!("{}█", p.editor_command)
                } else if p.editor_command.is_empty() {
                    "(empty)".to_string()
                } else {
                    p.editor_command.clone()
                };
                format!("    Editor command: {}", cmd)
            }
            5 => format!(
                " {} Open terminal in same directory as file explorer",
                if p.open_terminal_in_explorer_dir {
                    "[✓]"
                } else {
                    "[ ]"
                }
            ),
            6 => format!(
                " {} File explorer follows terminal directory changes",
                if p.explorer_follows_terminal {
                    "[✓]"
                } else {
                    "[ ]"
                }
            ),
            _ => String::new(),
        };

        let style = if editing_here {
            Style::default().fg(Color::White).bg(INPUT_BG)
        } else if is_selected {
            Style::default()
                .bg(MENU_SELECTED)
                .fg(Color::Black)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(TEXT)
        };

        let line_y = inner.y + 1 + i as u16;
        let line_area = Rect::new(inner.x + 1, line_y, inner.width.saturating_sub(2), 1);
        let para = Paragraph::new(Span::styled(
            if line.len() > inner.width.saturating_sub(2) as usize {
                let max = inner.width.saturating_sub(5) as usize;
                format!("{}…", line.chars().take(max).collect::<String>())
            } else {
                line
            },
            style,
        ));
        frame.render_widget(para, line_area);
    }

    let hint = Paragraph::new(Span::styled(
        " ↑↓ Tab:Next row Enter:Edit editor Space:Toggle Esc:Close ",
        Style::default().fg(TEXT_DIM),
    ))
    .alignment(Alignment::Center);
    let hint_y = area.y + dialog_height.saturating_sub(2);
    frame.render_widget(
        hint,
        Rect::new(area.x, hint_y, area.width, 1),
    );
}

fn draw_keyboard_shortcuts(frame: &mut Frame, app: &mut App) {
    let screen = frame.area();

    let dialog_width = 76u16.min(screen.width.saturating_sub(4));
    let dialog_height = 26u16
        .min(screen.height.saturating_sub(6))
        .max(14);

    let dialog_x = screen.width.saturating_sub(dialog_width) / 2;
    let dialog_y = screen.height.saturating_sub(dialog_height) / 2;

    let area = Rect::new(dialog_x, dialog_y, dialog_width, dialog_height);
    frame.render_widget(Clear, area);

    let dialog_block = Block::default()
        .title(Span::styled(
            " Shortcuts ",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(ACCENT))
        .style(Style::default().bg(PANEL_BG));

    let inner = dialog_block.inner(area);
    frame.render_widget(dialog_block, area);
    
    let shortcuts = vec![
        ("", "── Navigation ──"),
        ("↑/↓", "Move selection up/down"),
        ("Ctrl+Y/Ctrl+V", "Page up/down (explorer & scrollable dialogs)"),
        ("Tab", "Switch panel (local ↔ remote/connections)"),
        ("Shift+Tab", "Switch to previous panel or menu"),
        ("Enter", "Open file/folder or connect"),
        ("/", "Edit current path (Tab to autocomplete)"),
        ("/ /", "Double-tap to start from root (/)"),
        ("Esc", "Clear file selection if any; else menu / close dialog"),
        ("", ""),
        ("", "── Selection ──"),
        ("Space", "Toggle selection of current item"),
        ("Shift+↑/↓", "Range select multiple items"),
        ("", ""),
        ("", "── File Operations ──"),
        ("D", "Download selected (remote panel)"),
        ("U", "Upload selected (local panel)"),
        ("Z", "Zip selected files/folders"),
        ("Z Z", "Zip selection, then queue upload (local) or download (remote)"),
        ("X", "Delete all selected files/folders"),
        ("", ""),
        ("", "── Filtering ──"),
        (":", "Start text filter"),
        (";", "Start regex filter"),
        ("Backspace", "Remove filter character"),
        ("Esc", "Clear filter"),
        ("", ""),
        ("", "── Menu ──"),
        ("←/→", "Navigate menu tabs"),
        ("Enter/↓", "Open menu dropdown"),
        ("", ""),
        ("", "── Settings ──"),
        ("File → Settings", "Columns, editor, terminal cwd options"),
        ("", ""),
        ("", "── Terminal ──"),
        ("`", "Toggle and focus terminal"),
        ("Esc", "Unfocus terminal"),
        ("Shift+↑/↓", "Scroll terminal output"),
        ("PgUp/PgDn", "Scroll terminal output (fast)"),
        ("", ""),
        ("", "── This Dialog ──"),
        ("↑↓/j k", "Scroll shortcuts"),
        ("Ctrl+Y/Ctrl+V", "Page up/down"),
        ("PgUp/PgDn", "Page up/down"),
        ("Home/End", "Jump to start/end"),
        ("Esc", "Close"),
    ];
    
    let total_items = shortcuts.len();
    app.shortcuts_help_line_count = total_items;
    let visible_height = inner.height as usize;
    app.shortcuts_viewport_height = visible_height.max(1);
    app.clamp_shortcuts_scroll();
    let scroll_offset = app.shortcuts_scroll_offset;

    let max_key_width = 14;
    let text_width = inner.width.saturating_sub(3);

    if total_items > visible_height {
        // Ratatui's thumb reaches the end only when position == content_length - 1; it uses
        // end = position + viewport in the denominator. Our scroll model uses first-line index
        // (max total - visible), so map linearly into 0..=total_items-1.
        let max_scroll = total_items.saturating_sub(visible_height);
        let scrollbar_position = if max_scroll == 0 {
            0
        } else {
            scroll_offset.saturating_mul(total_items.saturating_sub(1)) / max_scroll
        };

        let scrollbar_area = Rect::new(inner.x + inner.width - 1, inner.y, 1, inner.height);
        let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .begin_symbol(Some("▲"))
            .end_symbol(Some("▼"))
            .track_symbol(Some("│"))
            .thumb_symbol("█");

        let mut scrollbar_state = ScrollbarState::new(total_items)
            .position(scrollbar_position)
            .viewport_content_length(visible_height.max(1));

        frame.render_stateful_widget(scrollbar, scrollbar_area, &mut scrollbar_state);
    }

    for (i, (key, desc)) in shortcuts
        .iter()
        .skip(scroll_offset)
        .take(visible_height)
        .enumerate()
    {
        let y = inner.y + i as u16;

        if key.is_empty() {
            let style = if desc.starts_with("──") {
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(TEXT_DIM)
            };
            let text = Paragraph::new(Span::styled(*desc, style));
            let text_area = Rect::new(inner.x + 1, y, text_width, 1);
            frame.render_widget(text, text_area);
        } else {
            let key_span = Span::styled(
                format!("{:>width$}", key, width = max_key_width),
                Style::default().fg(MENU_SELECTED).add_modifier(Modifier::BOLD),
            );
            let sep_span = Span::styled(" ", Style::default());
            let desc_span = Span::styled(*desc, Style::default().fg(TEXT));

            let line = Line::from(vec![key_span, sep_span, desc_span]);
            let text = Paragraph::new(line);
            let text_area = Rect::new(inner.x + 1, y, text_width, 1);
            frame.render_widget(text, text_area);
        }
    }
}

fn draw_status_bar(frame: &mut Frame, app: &App, area: Rect) {
    let status = if let Some(ref err) = app.error_message {
        Span::styled(format!(" {} ", err), Style::default().fg(ERROR))
    } else if let Some(ref msg) = app.status_message {
        Span::styled(format!(" {} ", msg), Style::default().fg(ACCENT))
    } else if !app.tabs.is_empty() {
        if let Some(tab) = app.current_tab() {
            Span::styled(format!(" {} ", tab.name), Style::default().fg(ACCENT))
        } else {
            Span::styled(" Ready ", Style::default().fg(ACCENT))
        }
    } else {
        Span::styled(" Ready ", Style::default().fg(ACCENT))
    };

    let transfer_status = {
        let items = app.transfer_manager.get_items();
        let active: Vec<_> = items.iter().filter(|i| {
            matches!(i.status, crate::transfer::TransferStatus::InProgress { .. } | 
                              crate::transfer::TransferStatus::Retrying { .. } |
                              crate::transfer::TransferStatus::Pending)
        }).collect();
        
        if !active.is_empty() {
            format!(" {} transfers ", active.len())
        } else {
            String::new()
        }
    };

    let hints = if app.terminal_focus != TerminalFocus::None {
        "Type to input | Shift+↑↓:Scroll | PgUp/PgDn:Scroll | Esc:Unfocus"
    } else {
        match app.mode {
            AppMode::Normal => match app.focus {
                FocusPanel::Local => "↑↓:Nav Enter:Open ::Filter ;:Regex Z:Zip X:Del /:Path Tab:Next area `:Term",
                FocusPanel::Remote => "↑↓:Nav Enter:Connect Tab:Next area",
                FocusPanel::ConnectionTabs => "Tab:Next area",
            },
            AppMode::MenuFocused => "←→:Menu Tab:Next area Enter:Open Esc:Close",
            AppMode::MenuOpen => "↑↓:Nav Enter:Select Esc:Close",
            AppMode::ConnectionDialog => "Tab:Next Enter:Connect Esc:Cancel",
            AppMode::ConnectionList => "↑↓:Nav Enter:Connect Esc:Close",
            AppMode::Connected => match app.focus {
                FocusPanel::Local => "↑↓:Nav Enter:Open ::Filter ;:Regex U:Upload Z:Zip X:Del /:Path Tab:Next area `:Term",
                FocusPanel::Remote => "↑↓:Nav Enter:Open ::Filter ;:Regex D:Download Z:Zip X:Del /:Path Tab:Next area `:Term",
                FocusPanel::ConnectionTabs => "←→:Select Enter:Switch session Tab:Next area Esc:Files",
            },
            AppMode::DirectoryInput => "Tab:Complete Enter:Go Esc:Cancel",
            AppMode::DeleteConfirm => "←→:Select Enter:Confirm Esc:Cancel",
            AppMode::Settings => "↑↓ Space:Toggle Enter:Edit editor Esc:Close",
            AppMode::KeyboardShortcuts => "↑↓ j/k Ctrl+Y/V PgUp/Dn Home/End Esc:Close",
        }
    };

    let status_line = Line::from(vec![
        status,
        Span::styled(transfer_status, Style::default().fg(MENU_SELECTED)),
        Span::styled(" │ ", Style::default().fg(BORDER)),
        Span::styled(hints, Style::default().fg(TEXT_DIM)),
    ]);

    let status_bar = Paragraph::new(status_line).style(Style::default().bg(MENU_BG));
    frame.render_widget(status_bar, area);
}
