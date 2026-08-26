mod app;
mod download;
mod qr;
mod storage;
mod ui;

use std::io::{self, Stdout};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use app::{App, InputMode, LoginPhase, NoticeKind};
use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use futures_util::StreamExt;
use netease_qq_music_api::MusicClient;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use storage::AppPaths;
use tokio::sync::mpsc;

#[tokio::main]
async fn main() -> Result<()> {
    AppPaths::enter_app_directory()?;
    let paths = AppPaths::discover()?;
    let client = Arc::new(MusicClient::new());
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut app = App::new(client, tx, paths);
    app.check_and_refresh_cookies();

    let mut terminal = setup_terminal()?;
    let _guard = TerminalGuard;
    let mut events = EventStream::new();
    let mut redraw = tokio::time::interval(Duration::from_millis(100));
    redraw.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    while !app.should_quit {
        terminal
            .draw(|frame| ui::draw(frame, &app))
            .context("绘制终端界面失败")?;

        tokio::select! {
            maybe_event = events.next() => {
                match maybe_event {
                    Some(Ok(Event::Key(key))) if key.kind == KeyEventKind::Press || key.kind == KeyEventKind::Repeat => {
                        handle_key(&mut app, key);
                    }
                    Some(Ok(_)) => {}
                    Some(Err(error)) => {
                        app.set_notice(NoticeKind::Error, format!("读取键盘事件失败:{error}"));
                    }
                    None => {
                        app.set_notice(NoticeKind::Error, "终端事件流已关闭");
                        app.should_quit = true;
                    }
                }
            }
            Some(message) = rx.recv() => app.handle_message(message),
            _ = redraw.tick() => {}
        }
    }

    app.cancel_login();
    terminal.show_cursor().ok();
    Ok(())
}

fn setup_terminal() -> Result<Terminal<CrosstermBackend<Stdout>>> {
    enable_raw_mode().context("无法启用终端原始模式")?;
    let mut stdout = io::stdout();
    if let Err(error) = execute!(stdout, EnterAlternateScreen) {
        let _ = disable_raw_mode();
        return Err(error).context("无法进入终端全屏模式");
    }
    Terminal::new(CrosstermBackend::new(stdout)).context("无法初始化终端界面")
}

struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
    }
}

fn handle_key(app: &mut App, key: KeyEvent) {
    let control = key.modifiers.contains(KeyModifiers::CONTROL);
    let control_char = match key.code {
        KeyCode::Char(character) if control => Some(character.to_ascii_lowercase()),
        _ => None,
    };

    if control_char == Some('c') {
        app.should_quit = true;
        return;
    }
    if control_char == Some('q') {
        app.request_quit();
        return;
    }

    if app.login_overlay.is_some() {
        match key.code {
            KeyCode::Esc => {
                app.cancel_login();
                app.set_notice(NoticeKind::Info, "已关闭登录窗口");
            }
            _ if control_char == Some('l')
                && app.login_overlay.as_ref().is_some_and(|overlay| {
                    matches!(overlay.phase, LoginPhase::Expired | LoginPhase::Failed)
                }) =>
            {
                app.start_login();
            }
            _ => {}
        }
        return;
    }

    if app.show_help {
        match key.code {
            KeyCode::Esc | KeyCode::F(1) => app.show_help = false,
            _ if control_char == Some('h') => app.show_help = false,
            _ => {}
        }
        return;
    }

    if key.code == KeyCode::F(1) || control_char == Some('h') {
        app.show_help = true;
        return;
    }
    if key.code == KeyCode::Tab || control_char == Some('p') {
        app.toggle_platform();
        return;
    }

    if let Some(shortcut) = control_char {
        match shortcut {
            'd' => app.queue_downloads(),
            'l' => app.start_login(),
            't' | 'x' => app.cycle_quality(),
            'r' => app.submit_search(),
            'a' if app.input_mode == InputMode::Normal => app.toggle_mark_page(),
            'u' if app.input_mode == InputMode::Editing => app.input.clear(),
            _ => {}
        }
        return;
    }

    if app.input_mode == InputMode::Editing {
        match key.code {
            KeyCode::Enter => app.submit_search(),
            KeyCode::Esc => {
                app.input.clone_from(&app.query);
                app.input_mode = InputMode::Normal;
                app.set_notice(NoticeKind::Info, "已取消编辑搜索词");
            }
            KeyCode::Backspace => {
                app.input.pop();
            }
            KeyCode::Delete => app.input.clear(),
            KeyCode::Char(character)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                app.input.push(character);
            }
            _ => {}
        }
        return;
    }

    match key.code {
        KeyCode::Up => app.select_previous(),
        KeyCode::Down => app.select_next(),
        KeyCode::Home => app.selected_row = 0,
        KeyCode::End => app.selected_row = app.results.len().saturating_sub(1),
        KeyCode::Left | KeyCode::PageUp => app.previous_page(),
        KeyCode::Right | KeyCode::PageDown => app.next_page(),
        KeyCode::Insert => app.toggle_mark(),
        KeyCode::F(8) => app.clear_finished_downloads(),
        KeyCode::Enter => app.submit_search(),
        KeyCode::Backspace => {
            app.input_mode = InputMode::Editing;
            app.input.pop();
        }
        KeyCode::Delete => {
            app.input_mode = InputMode::Editing;
            app.input.clear();
        }
        KeyCode::Char(character)
            if !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
        {
            app.input_mode = InputMode::Editing;
            app.input.push(character);
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use netease_qq_music_api::models::{LoginStatus, Platform, SongQuality};
    use std::path::PathBuf;

    fn test_app() -> App {
        let (tx, _rx) = mpsc::unbounded_channel();
        App::new(
            Arc::new(MusicClient::new()),
            tx,
            AppPaths {
                cookie_file: PathBuf::from("__dqn_test_cookie_does_not_exist.json"),
                download_dir: PathBuf::from("test-downloads"),
            },
        )
    }

    #[test]
    fn ordinary_letters_always_edit_the_search_query() {
        let mut app = test_app();
        app.input = "周杰伦".to_owned();
        app.input_mode = InputMode::Normal;

        handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE),
        );
        handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE),
        );

        assert_eq!(app.input, "周杰伦dq");
        assert_eq!(app.input_mode, InputMode::Editing);
        assert!(!app.should_quit);
        assert!(app.downloads.is_empty());
    }

    #[test]
    fn control_q_remains_an_explicit_exit_shortcut() {
        let mut app = test_app();
        app.input_mode = InputMode::Normal;

        handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL),
        );

        assert!(app.should_quit);
    }

    #[test]
    fn help_shortcuts_work_while_search_is_being_edited() {
        let mut app = test_app();
        assert_eq!(app.input_mode, InputMode::Editing);

        handle_key(&mut app, KeyEvent::new(KeyCode::F(1), KeyModifiers::NONE));
        assert!(app.show_help);

        handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('h'), KeyModifiers::CONTROL),
        );
        assert!(!app.show_help);
    }

    #[test]
    fn platform_and_quality_shortcuts_are_global() {
        let mut app = test_app();
        assert_eq!(app.platform, Platform::Netease);
        assert_eq!(app.quality, SongQuality::Lossless);

        handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL),
        );
        handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL),
        );

        assert_eq!(app.platform, Platform::Tencent);
        assert_eq!(app.quality, SongQuality::Hires);
        assert_eq!(app.input_mode, InputMode::Editing);
    }

    #[tokio::test(flavor = "current_thread")]
    #[ignore = "requires live QQ Music network services"]
    async fn qq_login_session_can_wait_for_scan_in_terminal() {
        let client = MusicClient::new();
        let session = tokio::time::timeout(
            Duration::from_secs(20),
            client.login().session().platform(Platform::Tencent).send(),
        )
        .await
        .expect("QQ login session timed out")
        .expect("QQ login session failed");
        let terminal_qr = crate::qr::prepare_qr(session.qr_code()).expect("QQ QR render failed");
        let qr_width = terminal_qr
            .iter()
            .map(|line| line.chars().count())
            .max()
            .unwrap_or(0);
        eprintln!(
            "QQ terminal QR: {} columns x {} rows",
            qr_width,
            terminal_qr.len()
        );
        assert!(
            qr_width <= 80,
            "QQ QR is too wide for an 80-column terminal"
        );
        assert!(
            terminal_qr.len() <= 24,
            "QQ QR is too tall for a 24-row terminal"
        );
        assert!(terminal_qr.iter().any(|line| line.contains('█')));

        let status = tokio::time::timeout(Duration::from_secs(20), session.status())
            .await
            .expect("QQ MQTT polling timed out")
            .expect("QQ MQTT polling failed");
        assert!(matches!(status, LoginStatus::WaitingScan));
    }
}
