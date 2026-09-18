use std::time::{SystemTime, UNIX_EPOCH};

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::prelude::Stylize;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Clear, Paragraph, Row, Table, TableState, Wrap};

use crate::app::{App, DownloadItem, DownloadState, InputMode, LoginPhase, NoticeKind, PAGE_SIZE};
use netease_qq_music_api::models::Platform;

const ACCENT: Color = Color::Rgb(118, 203, 255);
const SUCCESS: Color = Color::Rgb(112, 214, 153);
const WARNING: Color = Color::Rgb(255, 196, 92);
const ERROR: Color = Color::Rgb(255, 112, 122);
const MUTED: Color = Color::Rgb(130, 142, 158);

pub fn draw(frame: &mut Frame<'_>, app: &App) {
    let area = frame.area();
    if app.login_overlay.is_some() {
        draw_login(frame, app, area);
        return;
    }
    if area.width < 58 || area.height < 18 {
        draw_too_small(frame, area);
        return;
    }

    let cookie_warning_count = app.cookie_warnings().len() as u16;
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Length(3),
            Constraint::Min(7),
            Constraint::Length(7),
            Constraint::Length(4 + cookie_warning_count),
        ])
        .split(area);

    draw_header(frame, app, chunks[0]);
    draw_search(frame, app, chunks[1]);
    draw_results(frame, app, chunks[2]);
    draw_downloads(frame, app, chunks[3]);
    draw_footer(frame, app, chunks[4]);

    if app.show_help {
        draw_help(frame, area);
    }
}

fn draw_header(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let netease_login = app.login_state(Platform::Netease);
    let tencent_login = app.login_state(Platform::Tencent);
    let login_style = |login| match login {
        "已登录" => Style::default().fg(SUCCESS),
        "登录已过期" | "登录待确认" => Style::default().fg(WARNING),
        _ => Style::default().fg(MUTED),
    };
    let line = Line::from(vec![
        Span::styled(" dqn ", Style::default().fg(Color::Black).bg(ACCENT).bold()),
        Span::raw("  平台 "),
        Span::styled(
            App::platform_label(app.platform),
            Style::default().fg(ACCENT).bold(),
        ),
        Span::raw("  登录 网:"),
        Span::styled(netease_login, login_style(netease_login)),
        Span::raw(" Q:"),
        Span::styled(tencent_login, login_style(tencent_login)),
        Span::raw("  音质 "),
        Span::styled(app.quality_label(), Style::default().fg(WARNING)),
        Span::raw(format!("  已选 {} 首", app.marked.len())),
    ]);
    frame.render_widget(
        Paragraph::new(line).block(Block::default().borders(Borders::ALL).title(" 音乐下载器 ")),
        area,
    );
}

fn draw_search(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let prompt_style = if app.input_mode == InputMode::Editing {
        Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(MUTED)
    };
    let loading = if app.loading {
        format!("  {} 搜索中...", spinner())
    } else {
        String::new()
    };
    let line = Line::from(vec![
        Span::styled("> ", prompt_style),
        Span::styled(app.input.as_str(), prompt_style),
        Span::styled(loading, Style::default().fg(WARNING)),
    ]);
    let title = if app.input_mode == InputMode::Editing {
        " 搜索(输入后 Enter,Esc 取消) "
    } else {
        " 搜索(直接输入文字即可修改) "
    };
    frame.render_widget(
        Paragraph::new(line).block(Block::default().borders(Borders::ALL).title(title)),
        area,
    );
}

fn draw_results(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let header = Row::new(["", "序号", "歌曲", "歌手", "专辑"])
        .style(Style::default().fg(ACCENT).bold())
        .bottom_margin(1);
    let rows = app.results.iter().enumerate().map(|(index, song)| {
        let artists = song
            .artists
            .iter()
            .map(|artist| artist.name.as_str())
            .collect::<Vec<_>>()
            .join(",");
        let marked = if app.is_marked(song) { "*" } else { " " };
        Row::new(vec![
            Cell::from(marked),
            Cell::from(((app.page - 1) * PAGE_SIZE + index as u64 + 1).to_string()),
            Cell::from(song.name.clone()),
            Cell::from(artists),
            Cell::from(song.album.name.clone()),
        ])
    });

    let end = if app.results.is_empty() {
        0
    } else {
        (app.page - 1) * PAGE_SIZE + app.results.len() as u64
    };
    let next = if app.more {
        " - 还有下一页"
    } else {
        " - 已到末页"
    };
    let title = format!(" 搜索结果 - 第 {} 页 - 至 {end}{next} ", app.page);
    let table = Table::new(
        rows,
        [
            Constraint::Length(2),
            Constraint::Length(5),
            Constraint::Percentage(35),
            Constraint::Percentage(28),
            Constraint::Min(20),
        ],
    )
    .header(header)
    .block(Block::default().borders(Borders::ALL).title(title))
    .row_highlight_style(
        Style::default()
            .bg(Color::Rgb(42, 58, 74))
            .fg(Color::White)
            .bold(),
    )
    .highlight_symbol(">");
    let mut state =
        TableState::default().with_selected((!app.results.is_empty()).then_some(app.selected_row));
    frame.render_stateful_widget(table, area, &mut state);
}

fn draw_downloads(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let rows = app.downloads.iter().rev().take(4).map(download_row);
    let table = Table::new(
        rows,
        [
            Constraint::Length(3),
            Constraint::Percentage(38),
            Constraint::Percentage(43),
            Constraint::Percentage(19),
        ],
    )
    .header(Row::new(["", "歌曲", "状态", "进度"]).style(Style::default().fg(ACCENT).bold()))
    .block(Block::default().borders(Borders::ALL).title(format!(
        " 下载任务 {} - 保存到 {} ",
        app.downloads.len(),
        app.paths.download_dir.display()
    )));
    frame.render_widget(table, area);
}

fn download_row(item: &DownloadItem) -> Row<'static> {
    let quality = item
        .actual_quality
        .as_ref()
        .unwrap_or(&item.requested_quality);
    let quality = App::quality_name(quality);
    let (icon, status, style) = match &item.state {
        DownloadState::Queued => (
            ".",
            format!("等待并发槽位 - 请求{quality}"),
            Style::default().fg(MUTED),
        ),
        DownloadState::Resolving => (
            ">",
            format!("正在获取播放链接 - 请求{quality}"),
            Style::default().fg(WARNING),
        ),
        DownloadState::Downloading => (
            "v",
            format!("正在下载 - 实际{quality}"),
            Style::default().fg(ACCENT),
        ),
        DownloadState::Completed(path) => (
            "*",
            format!(
                "完成 - {quality} - {}",
                path.file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("文件")
            ),
            Style::default().fg(SUCCESS),
        ),
        DownloadState::Failed(error) => {
            ("!", format!("失败 - {error}"), Style::default().fg(ERROR))
        }
    };
    Row::new(vec![
        Cell::from(icon.to_owned()),
        Cell::from(format!(
            "[{}] {}",
            match item.platform {
                Platform::Netease => "网",
                Platform::Tencent => "Q",
            },
            item.song.name
        )),
        Cell::from(status),
        Cell::from(progress_text(item)),
    ])
    .style(style)
}

fn progress_text(item: &DownloadItem) -> String {
    match item.total {
        Some(total) if total > 0 => {
            let percent = (item.received.saturating_mul(100) / total).min(100);
            format!("{percent:>3}% {}", human_bytes(item.received))
        }
        _ if item.received > 0 => human_bytes(item.received),
        _ => "-".to_owned(),
    }
}

fn draw_footer(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let mut notice_style = match app.notice.kind {
        NoticeKind::Info => Style::default().fg(ACCENT),
        NoticeKind::Success => Style::default().fg(SUCCESS),
        NoticeKind::Warning => Style::default().fg(WARNING),
        NoticeKind::Error => Style::default().fg(ERROR),
    };
    if app.notice.changed_at.elapsed().as_secs() > 15 {
        notice_style = Style::default().fg(MUTED);
    }
    let shortcut = Line::from(vec![
        Span::styled("Ins", Style::default().fg(ACCENT).bold()),
        Span::raw("选择 "),
        Span::styled("^A", Style::default().fg(ACCENT).bold()),
        Span::raw("全选 "),
        Span::styled("<>", Style::default().fg(ACCENT).bold()),
        Span::raw("翻页 "),
        Span::styled("^D", Style::default().fg(SUCCESS).bold()),
        Span::raw("下载 "),
        Span::styled("^P", Style::default().fg(ACCENT).bold()),
        Span::raw("平台 "),
        Span::styled("^T", Style::default().fg(WARNING).bold()),
        Span::raw("音质 "),
        Span::styled("^L", Style::default().fg(WARNING).bold()),
        Span::raw("登录 "),
        Span::styled("^H", Style::default().fg(ACCENT).bold()),
        Span::raw("帮助 "),
        Span::styled("^Q", Style::default().fg(ERROR).bold()),
        Span::raw("退出"),
    ]);
    let notice = Line::from(vec![
        Span::styled("提示:", Style::default().fg(MUTED)),
        Span::styled(&app.notice.text, notice_style),
    ]);
    let mut lines: Vec<Line<'_>> = app
        .cookie_warnings()
        .into_iter()
        .map(|warning| Line::styled(warning, Style::default().fg(WARNING).bold()))
        .collect();
    lines.extend([shortcut, notice]);
    frame.render_widget(
        Paragraph::new(lines).block(Block::default().borders(Borders::ALL)),
        area,
    );
}

fn draw_help(frame: &mut Frame<'_>, area: Rect) {
    let popup = centered(area, 72, 22);
    frame.render_widget(Clear, popup);
    let help = vec![
        Line::styled("dqn 操作帮助", Style::default().fg(ACCENT).bold()),
        Line::raw(""),
        Line::raw("普通文字键     直接编辑搜索词;Enter 搜索;Esc 取消编辑"),
        Line::raw("Backspace/Del  修改或清空搜索词;Ctrl+U 也可清空"),
        Line::raw("Up/Down        移动当前行"),
        Line::raw("Insert         选择/取消当前歌曲"),
        Line::raw("Ctrl+A         选择/取消当前页全部歌曲"),
        Line::raw("Left/Right     上一页/下一页;Ctrl+R 刷新当前页"),
        Line::raw("Ctrl+P / Tab   在网易云音乐与 QQ 音乐间切换"),
        Line::raw("Ctrl+T / Ctrl+X 循环切换下载音质"),
        Line::raw("Ctrl+D         下载已选歌曲;未多选时下载当前行"),
        Line::raw("F8             清理完成和失败的下载记录"),
        Line::raw("Ctrl+L         为当前平台扫码登录"),
        Line::raw("Ctrl+Q         安全退出;Ctrl+C 强制退出"),
        Line::raw(""),
        Line::styled(
            "下载最多 3 路并发;受版权或会员限制的歌曲可能无可用链接.",
            Style::default().fg(WARNING),
        ),
        Line::raw("按 Esc / F1 / Ctrl+H 关闭帮助"),
    ];
    frame.render_widget(
        Paragraph::new(help)
            .block(Block::default().borders(Borders::ALL).title(" 帮助 "))
            .wrap(Wrap { trim: false }),
        popup,
    );
}

fn draw_login(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let Some(login) = app.login_overlay.as_ref() else {
        return;
    };
    // 登录界面独占整屏,确保不会残留上一帧的"小窗口"或主界面文字.
    frame.render_widget(Clear, area);
    frame.render_widget(
        Block::default().style(Style::default().fg(Color::White).bg(Color::Black)),
        area,
    );
    let qr_width = login
        .qr_lines
        .iter()
        .map(|line| line.chars().count() as u16)
        .max()
        .unwrap_or(0);
    let qr_fits_framed = qr_width.saturating_add(4) <= area.width
        && (login.qr_lines.len() as u16).saturating_add(10) <= area.height;
    let qr_fits_compact = qr_width <= area.width && login.qr_lines.len() as u16 <= area.height;
    if !qr_fits_framed
        && qr_fits_compact
        && login.phase == LoginPhase::WaitingScan
        && !login.qr_lines.is_empty()
    {
        draw_compact_login_qr(frame, login, area, qr_width);
        return;
    }

    let desired_height = if qr_fits_framed {
        (login.qr_lines.len() as u16 + 8).max(11)
    } else {
        12
    };
    let desired_width = if qr_fits_framed {
        qr_width.saturating_add(2).max(68)
    } else {
        68
    };
    let popup = centered(area, desired_width, desired_height);
    frame.render_widget(Clear, popup);
    let phase_style = match login.phase {
        LoginPhase::Creating | LoginPhase::WaitingScan => Style::default().fg(ACCENT),
        LoginPhase::WaitingConfirm => Style::default().fg(SUCCESS).bold(),
        LoginPhase::Expired => Style::default().fg(WARNING),
        LoginPhase::Failed => Style::default().fg(ERROR),
    };
    let mut lines = vec![
        Line::styled(login.detail.clone(), phase_style),
        Line::raw(""),
    ];
    if qr_fits_framed {
        lines.extend(
            login
                .qr_lines
                .iter()
                .cloned()
                .map(|line| Line::styled(line, Style::default().fg(Color::Black).bg(Color::White))),
        );
    } else if !login.qr_lines.is_empty() {
        lines.push(Line::styled(
            "当前终端不足以完整显示二维码,请放大终端窗口后重试.",
            Style::default().fg(WARNING),
        ));
        lines.push(Line::raw("为保证可扫描,不会显示残缺或缩放后的二维码."));
    }
    lines.push(Line::raw(""));
    lines.push(Line::styled(
        "Esc 关闭 - Ctrl+L 重新生成",
        Style::default().fg(MUTED),
    ));
    frame.render_widget(
        Paragraph::new(lines).alignment(Alignment::Center).block(
            Block::default()
                .borders(Borders::ALL)
                .title(format!(" {} 扫码登录 ", App::platform_label(app.platform))),
        ),
        popup,
    );
}

fn draw_compact_login_qr(
    frame: &mut Frame<'_>,
    login: &crate::app::LoginOverlay,
    area: Rect,
    qr_width: u16,
) {
    frame.render_widget(Clear, area);
    frame.render_widget(
        Block::default().style(Style::default().bg(Color::White)),
        area,
    );
    let qr_height = login.qr_lines.len() as u16;
    let qr_area = Rect {
        x: area.x + area.width.saturating_sub(qr_width) / 2,
        y: area.y + area.height.saturating_sub(qr_height) / 2,
        width: qr_width,
        height: qr_height,
    };
    let lines = login.qr_lines.iter().cloned().map(|line| {
        Line::styled(
            line,
            Style::default().fg(Color::Black).bg(Color::White).bold(),
        )
    });
    frame.render_widget(Paragraph::new(lines.collect::<Vec<_>>()), qr_area);
}

fn draw_too_small(frame: &mut Frame<'_>, area: Rect) {
    frame.render_widget(
        Paragraph::new("终端窗口太小\n请调整到至少 58 x 18 后继续")
            .alignment(Alignment::Center)
            .block(Block::default().borders(Borders::ALL).title(" dqn ")),
        area,
    );
}

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width.saturating_sub(2)).max(1);
    let height = height.min(area.height.saturating_sub(2)).max(1);
    Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    }
}

fn spinner() -> &'static str {
    const FRAMES: [&str; 4] = ["|", "/", "-", "\\"];
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or_default();
    FRAMES[(millis / 150 % FRAMES.len() as u128) as usize]
}

pub fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KiB", "MiB", "GiB"];
    let mut value = bytes as f64;
    let mut unit = 0usize;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use netease_qq_music_api::MusicClient;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use tokio::sync::mpsc;

    use crate::app::{LoginOverlay, LoginPhase};
    use crate::storage::AppPaths;

    fn cookie_test_app() -> App {
        let (tx, _rx) = mpsc::unbounded_channel();
        App::new(
            Arc::new(MusicClient::new()),
            tx,
            AppPaths {
                cookie_file: "__dqn_ui_test_cookie.json".into(),
                download_dir: "test-downloads".into(),
            },
        )
    }

    fn render_text(app: &App, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| draw(frame, app)).unwrap();
        terminal
            .backend()
            .buffer()
            .content()
            .chunks(width as usize)
            .map(|row| {
                let mut text = String::new();
                let mut column = 0;
                while column < row.len() {
                    let symbol = row[column].symbol();
                    text.push_str(symbol);
                    column += Line::from(symbol).width().max(1);
                }
                text
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn set_test_cookie(app: &mut App, expires_at: i64) {
        use netease_qq_music_api::models::{LoginToken, NeteaseLoginToken};
        app.cookies.set(LoginToken::Netease(NeteaseLoginToken::new(
            "test",
            "test",
            "test",
            Some(expires_at),
        )));
    }

    #[test]
    fn expired_cookie_warning_survives_other_notices_and_redraws() {
        let mut app = cookie_test_app();
        set_test_cookie(&mut app, i64::MAX);
        assert!(app.cookie_warnings().is_empty());

        // The next redraw must detect expiry without a startup refresh or an API error.
        set_test_cookie(&mut app, 1);
        app.set_notice(NoticeKind::Success, "搜索成功");
        app.notice.changed_at = std::time::Instant::now() - std::time::Duration::from_secs(60);
        let screen = render_text(&app, 80, 24);
        assert!(screen.contains("Cookie 已过期"));
        assert!(screen.contains("Ctrl+L"));
        assert!(screen.contains("搜索成功"));
        assert!(app.cookies.token_for(Platform::Netease).is_none());
    }

    #[test]
    fn refresh_failure_is_persistent_without_claiming_network_failure_is_expiry() {
        let mut app = cookie_test_app();
        set_test_cookie(&mut app, i64::MAX);
        app.handle_message(crate::app::AppMessage::CookieRefreshFinished {
            platform: Platform::Netease,
            result: Err("连接超时".to_owned()),
        });
        app.set_notice(NoticeKind::Success, "下载完成");
        assert_eq!(app.login_state(Platform::Netease), "登录待确认");
        assert!(app.cookies.token_for(Platform::Netease).is_some());
        let screen = render_text(&app, 80, 24);
        assert!(screen.contains("Cookie 刷新失败"));
        assert!(!screen.contains("Cookie 已过期"));
    }

    #[test]
    fn both_platform_warnings_fit_and_explain_how_to_switch_platform() {
        use netease_qq_music_api::models::{LoginToken, TencentLoginToken};
        let mut app = cookie_test_app();
        set_test_cookie(&mut app, 1);
        app.cookies.set(LoginToken::Tencent(TencentLoginToken::new(
            1,
            "test",
            "test",
            "test",
            Some(1),
            1,
        )));
        for (width, height) in [(58, 18), (58, 24), (80, 24)] {
            let screen = render_text(&app, width, height);
            assert_eq!(screen.matches("Cookie 已过期").count(), 2);
            assert!(screen.contains("Ctrl+P"));
            assert_eq!(screen.matches("Ctrl+L").count(), 2);
        }
    }

    #[test]
    fn successful_refresh_clears_only_its_platform_warning() {
        use netease_qq_music_api::models::{LoginToken, NeteaseLoginToken};
        let mut app = cookie_test_app();
        // A directory makes saving fail without creating any credentials on disk.
        app.paths.cookie_file = std::env::current_dir().unwrap();
        for platform in [Platform::Netease, Platform::Tencent] {
            app.handle_message(crate::app::AppMessage::CookieRefreshFinished {
                platform,
                result: Err("刷新失败".to_owned()),
            });
        }
        app.handle_message(crate::app::AppMessage::CookieRefreshFinished {
            platform: Platform::Netease,
            result: Ok(LoginToken::Netease(NeteaseLoginToken::new(
                "new",
                "new",
                "new",
                Some(i64::MAX),
            ))),
        });
        assert_eq!(app.login_state(Platform::Netease), "已登录");
        let warnings = app.cookie_warnings();
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("QQ音乐"));
    }

    #[test]
    fn successful_login_clears_expiry_and_refresh_failure_warning() {
        use netease_qq_music_api::models::{LoginToken, NeteaseLoginToken};
        let mut app = cookie_test_app();
        app.paths.cookie_file = std::env::current_dir().unwrap();
        set_test_cookie(&mut app, 1);
        app.handle_message(crate::app::AppMessage::CookieRefreshFinished {
            platform: Platform::Netease,
            result: Err("刷新失败".to_owned()),
        });
        app.login_overlay = Some(LoginOverlay {
            request_id: 1,
            phase: LoginPhase::WaitingConfirm,
            qr_lines: Vec::new(),
            detail: String::new(),
        });
        app.handle_message(crate::app::AppMessage::LoginSuccess {
            request_id: 1,
            token: LoginToken::Netease(NeteaseLoginToken::new("new", "new", "new", Some(i64::MAX))),
        });
        assert!(app.cookie_warnings().is_empty());
        assert_eq!(app.login_state(Platform::Netease), "已登录");
    }

    #[test]
    fn compact_qr_keeps_its_first_and_last_rows_on_an_80_by_24_terminal() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = App::new(
            Arc::new(MusicClient::new()),
            tx,
            AppPaths {
                cookie_file: "__dqn_ui_test_cookie.json".into(),
                download_dir: "test-downloads".into(),
            },
        );
        app.login_overlay = Some(LoginOverlay {
            request_id: 1,
            phase: LoginPhase::WaitingScan,
            qr_lines: vec!["████".to_owned(); 21],
            detail: "等待扫码".to_owned(),
        });

        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal
            .draw(|frame| draw_login(frame, &app, frame.area()))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let x = (80 - 4) / 2;
        let first_y = (24 - 21) / 2;
        let last_y = first_y + 20;

        assert_eq!(buffer.cell((x, first_y)).unwrap().symbol(), "█");
        assert_eq!(buffer.cell((x, last_y)).unwrap().symbol(), "█");
    }

    #[test]
    fn login_qr_has_priority_over_the_minimum_main_window_size() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = App::new(
            Arc::new(MusicClient::new()),
            tx,
            AppPaths {
                cookie_file: "__dqn_ui_test_cookie.json".into(),
                download_dir: "test-downloads".into(),
            },
        );
        let mut terminal = Terminal::new(TestBackend::new(50, 17)).unwrap();
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        assert!(
            terminal
                .backend()
                .buffer()
                .content()
                .iter()
                .any(|cell| cell.symbol() == "终")
        );

        app.login_overlay = Some(LoginOverlay {
            request_id: 1,
            phase: LoginPhase::WaitingScan,
            qr_lines: vec!["████".to_owned(); 15],
            detail: "等待扫码".to_owned(),
        });

        terminal.draw(|frame| draw(frame, &app)).unwrap();
        let content = terminal.backend().buffer().content();
        assert!(content.iter().any(|cell| cell.symbol() == "█"));
        assert!(!content.iter().any(|cell| cell.symbol() == "终"));
    }
}
