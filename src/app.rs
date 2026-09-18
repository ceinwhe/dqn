use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use netease_qq_music_api::models::{
    LoginStatus, LoginToken, Platform, SearchSongResult, Song, SongQuality, UrlResult,
};
use netease_qq_music_api::{MusicClient, MusicClientError};
use tokio::sync::{Semaphore, mpsc, oneshot};

use crate::download;
use crate::qr::{prepare_qr, validate_qr_payload};
use crate::storage::{AppPaths, CookieStore};

pub const PAGE_SIZE: u64 = 20;
const MAX_CONCURRENT_DOWNLOADS: usize = 3;
const COOKIE_REFRESH_BEFORE: Duration = Duration::from_secs(24 * 60 * 60);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InputMode {
    Normal,
    Editing,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NoticeKind {
    Info,
    Success,
    Warning,
    Error,
}

#[derive(Clone, Debug)]
pub struct Notice {
    pub text: String,
    pub kind: NoticeKind,
    pub changed_at: Instant,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LoginPhase {
    Creating,
    WaitingScan,
    WaitingConfirm,
    Expired,
    Failed,
}

#[derive(Clone, Debug)]
pub struct LoginOverlay {
    pub request_id: u64,
    pub phase: LoginPhase,
    pub qr_lines: Vec<String>,
    pub detail: String,
}

#[derive(Clone, Debug)]
pub struct MarkedSong {
    pub platform: Platform,
    pub song: Song,
}

#[derive(Clone, Debug)]
pub enum DownloadState {
    Queued,
    Resolving,
    Downloading,
    Completed(PathBuf),
    Failed(String),
}

#[derive(Clone, Debug)]
pub struct DownloadItem {
    pub job_id: u64,
    pub key: String,
    pub platform: Platform,
    pub song: Song,
    pub requested_quality: SongQuality,
    pub actual_quality: Option<SongQuality>,
    pub received: u64,
    pub total: Option<u64>,
    pub state: DownloadState,
}

pub enum AppMessage {
    SearchFinished {
        request_id: u64,
        result: Result<SearchSongResult, String>,
    },
    LoginQr {
        request_id: u64,
        payload: String,
    },
    LoginPhase {
        request_id: u64,
        phase: LoginPhase,
        detail: String,
    },
    LoginSuccess {
        request_id: u64,
        token: LoginToken,
    },
    CookieRefreshFinished {
        platform: Platform,
        cookie_revision: u64,
        result: Result<LoginToken, String>,
    },
    DownloadStage {
        job_id: u64,
        state: DownloadState,
    },
    DownloadResolved {
        job_id: u64,
        actual_quality: SongQuality,
    },
    DownloadProgress {
        job_id: u64,
        received: u64,
        total: Option<u64>,
    },
}

pub struct App {
    pub client: Arc<MusicClient>,
    pub tx: mpsc::UnboundedSender<AppMessage>,
    pub paths: AppPaths,
    pub cookies: CookieStore,
    pub platform: Platform,
    pub quality: SongQuality,
    pub query: String,
    pub input: String,
    pub input_mode: InputMode,
    pub results: Vec<Song>,
    pub selected_row: usize,
    pub marked: HashMap<String, MarkedSong>,
    pub page: u64,
    pub more: bool,
    pub loading: bool,
    pub search_request_id: u64,
    pub downloads: Vec<DownloadItem>,
    pub show_help: bool,
    pub login_overlay: Option<LoginOverlay>,
    pub should_quit: bool,
    pub notice: Notice,
    download_semaphore: Arc<Semaphore>,
    next_job_id: u64,
    login_request_id: u64,
    login_cancel: Option<oneshot::Sender<()>>,
    cookie_refresh_pending: usize,
    cookie_refresh_failed: Vec<Platform>,
    cookie_revisions: [u64; 2],
}

impl App {
    pub fn new(
        client: Arc<MusicClient>,
        tx: mpsc::UnboundedSender<AppMessage>,
        paths: AppPaths,
    ) -> Self {
        let (cookies, notice) = match CookieStore::load(&paths.cookie_file) {
            Ok(cookies) => (
                cookies,
                Notice {
                    text: "直接输入关键词后按 Enter 搜索;按 F1 或 Ctrl+H 查看帮助".to_owned(),
                    kind: NoticeKind::Info,
                    changed_at: Instant::now(),
                },
            ),
            Err(error) => (
                CookieStore::default(),
                Notice {
                    text: format!("登录信息读取失败,将以未登录状态启动:{error}"),
                    kind: NoticeKind::Warning,
                    changed_at: Instant::now(),
                },
            ),
        };

        Self {
            client,
            tx,
            paths,
            cookies,
            platform: Platform::Netease,
            quality: SongQuality::Lossless,
            query: String::new(),
            input: String::new(),
            input_mode: InputMode::Editing,
            results: Vec::new(),
            selected_row: 0,
            marked: HashMap::new(),
            page: 1,
            more: false,
            loading: false,
            search_request_id: 0,
            downloads: Vec::new(),
            show_help: false,
            login_overlay: None,
            should_quit: false,
            notice,
            download_semaphore: Arc::new(Semaphore::new(MAX_CONCURRENT_DOWNLOADS)),
            next_job_id: 1,
            login_request_id: 0,
            login_cancel: None,
            cookie_refresh_pending: 0,
            cookie_refresh_failed: Vec::new(),
            cookie_revisions: [0; 2],
        }
    }

    /// 启动时检查已保存 Token 的有效期,并在过期前自动刷新.
    pub fn check_and_refresh_cookies(&mut self) {
        if self.cookie_refresh_pending > 0 {
            return;
        }

        let stored_count = self.cookies.stored_count();
        if stored_count == 0 {
            return;
        }
        let candidates = self.cookies.tokens_requiring_refresh(COOKIE_REFRESH_BEFORE);
        if candidates.is_empty() {
            self.set_notice(
                NoticeKind::Success,
                format!("已检查 {stored_count} 个登录 Cookie,均在有效期内"),
            );
            return;
        }

        self.cookie_refresh_pending = candidates.len();
        self.set_notice(
            NoticeKind::Info,
            format!(
                "正在自动更新 {} 个已过期,即将过期或有效期未知的 Cookie...",
                candidates.len()
            ),
        );
        for (platform, token) in candidates {
            let cookie_revision = self.cookie_revisions[platform_index(platform)];
            let client = Arc::clone(&self.client);
            let tx = self.tx.clone();
            tokio::spawn(async move {
                let result = match &token {
                    LoginToken::Netease(token) => {
                        client
                            .login()
                            .refresh()
                            .platform(platform)
                            .token(token)
                            .send()
                            .await
                    }
                    LoginToken::Tencent(token) => {
                        client
                            .login()
                            .refresh()
                            .platform(platform)
                            .token(token)
                            .send()
                            .await
                    }
                }
                .map_err(|error| error.to_string());
                let _ = tx.send(AppMessage::CookieRefreshFinished {
                    platform,
                    cookie_revision,
                    result,
                });
            });
        }
    }

    pub fn set_notice(&mut self, kind: NoticeKind, text: impl Into<String>) {
        self.notice = Notice {
            text: text.into(),
            kind,
            changed_at: Instant::now(),
        };
    }

    pub fn login_state(&self, platform: Platform) -> &'static str {
        if self.cookies.is_expired(platform) {
            "登录已过期"
        } else if self.cookie_refresh_failed.contains(&platform) {
            "登录待确认"
        } else {
            self.cookies.login_state(platform)
        }
    }

    /// 登录问题独立于瞬时操作提示,直到刷新或重新登录成功才消失.
    /// 每次绘制都检查本地有效期,覆盖程序运行期间 Cookie 到期的情况.
    pub fn cookie_warnings(&self) -> Vec<String> {
        [Platform::Netease, Platform::Tencent]
            .into_iter()
            .filter_map(|platform| {
                let problem = if self.cookies.is_expired(platform) {
                    "Cookie 已过期"
                } else if self.cookie_refresh_failed.contains(&platform) {
                    "Cookie 刷新失败"
                } else {
                    return None;
                };
                let name = match platform {
                    Platform::Netease => "网易云",
                    Platform::Tencent => "QQ音乐",
                };
                let action = if platform == self.platform {
                    "Ctrl+L 重新登录"
                } else {
                    "Ctrl+P 切换后 Ctrl+L 登录"
                };
                Some(format!("{name} {problem};{action}"))
            })
            .collect()
    }

    pub fn platform_label(platform: Platform) -> &'static str {
        match platform {
            Platform::Netease => "网易云音乐",
            Platform::Tencent => "QQ 音乐",
        }
    }

    pub fn quality_label(&self) -> &'static str {
        Self::quality_name(&self.quality)
    }

    pub fn quality_name(quality: &SongQuality) -> &'static str {
        match quality {
            SongQuality::Master => "母带",
            SongQuality::Surround => "环绕声",
            SongQuality::Stereo => "立体声",
            SongQuality::Hires => "Hi-Res",
            SongQuality::Lossless => "无损",
            SongQuality::Exhigh => "极高",
            SongQuality::Standard => "标准",
        }
    }

    pub fn cycle_quality(&mut self) {
        self.quality = match self.quality {
            SongQuality::Standard => SongQuality::Exhigh,
            SongQuality::Exhigh => SongQuality::Lossless,
            SongQuality::Lossless => SongQuality::Hires,
            SongQuality::Hires => SongQuality::Stereo,
            SongQuality::Stereo => SongQuality::Surround,
            SongQuality::Surround => SongQuality::Master,
            SongQuality::Master => SongQuality::Standard,
        };
        self.set_notice(
            NoticeKind::Info,
            format!("下载音质已切换为 {}", self.quality_label()),
        );
    }

    pub fn toggle_platform(&mut self) {
        // Invalidate the old platform even when an empty input prevents a new search.
        self.search_request_id += 1;
        self.loading = false;
        self.platform = match self.platform {
            Platform::Netease => Platform::Tencent,
            Platform::Tencent => Platform::Netease,
        };
        self.page = 1;
        self.selected_row = 0;
        self.results.clear();
        self.more = false;
        self.set_notice(
            NoticeKind::Info,
            format!("已切换到 {}", Self::platform_label(self.platform)),
        );
        if !self.query.trim().is_empty() {
            self.submit_search();
        }
    }

    pub fn submit_search(&mut self) {
        let keyword = self.input.trim().to_owned();
        if keyword.is_empty() {
            self.set_notice(NoticeKind::Warning, "请输入搜索关键词");
            self.input_mode = InputMode::Editing;
            return;
        }

        if keyword != self.query {
            self.page = 1;
        }
        self.query = keyword.clone();
        self.input = keyword.clone();
        self.loading = true;
        self.input_mode = InputMode::Normal;
        self.search_request_id += 1;
        let request_id = self.search_request_id;
        let platform = self.platform;
        let offset = (self.page - 1) * PAGE_SIZE;
        let client = Arc::clone(&self.client);
        let token = self.cookies.token_for(platform);
        let tx = self.tx.clone();
        self.set_notice(
            NoticeKind::Info,
            format!("正在搜索\"{keyword}\"第 {} 页...", self.page),
        );

        tokio::spawn(async move {
            let result = search_songs(&client, &keyword, platform, offset, token.as_ref())
                .await
                .map_err(|error| error.to_string());
            let _ = tx.send(AppMessage::SearchFinished { request_id, result });
        });
    }

    pub fn previous_page(&mut self) {
        if self.loading {
            self.set_notice(NoticeKind::Info, "搜索仍在进行,请稍候");
        } else if self.page > 1 {
            self.page -= 1;
            self.submit_search();
        } else {
            self.set_notice(NoticeKind::Info, "已经是第一页");
        }
    }

    pub fn next_page(&mut self) {
        if self.loading {
            self.set_notice(NoticeKind::Info, "搜索仍在进行,请稍候");
        } else if self.more {
            self.page += 1;
            self.submit_search();
        } else {
            self.set_notice(NoticeKind::Info, "已经是最后一页");
        }
    }

    pub fn select_previous(&mut self) {
        if self.results.is_empty() {
            return;
        }
        self.selected_row = self.selected_row.saturating_sub(1);
    }

    pub fn select_next(&mut self) {
        if self.results.is_empty() {
            return;
        }
        self.selected_row = (self.selected_row + 1).min(self.results.len() - 1);
    }

    pub fn toggle_mark(&mut self) {
        let Some(song) = self.results.get(self.selected_row).cloned() else {
            self.set_notice(NoticeKind::Info, "当前没有可选择的歌曲");
            return;
        };
        let key = song_key(self.platform, &song.id);
        if self.marked.remove(&key).is_none() {
            self.marked.insert(
                key,
                MarkedSong {
                    platform: self.platform,
                    song,
                },
            );
        }
        self.set_notice(
            NoticeKind::Info,
            format!("已选择 {} 首歌曲", self.marked.len()),
        );
    }

    pub fn toggle_mark_page(&mut self) {
        if self.results.is_empty() {
            self.set_notice(NoticeKind::Info, "当前页没有歌曲");
            return;
        }
        let all_marked = self
            .results
            .iter()
            .all(|song| self.marked.contains_key(&song_key(self.platform, &song.id)));
        for song in &self.results {
            let key = song_key(self.platform, &song.id);
            if all_marked {
                self.marked.remove(&key);
            } else {
                self.marked.insert(
                    key,
                    MarkedSong {
                        platform: self.platform,
                        song: song.clone(),
                    },
                );
            }
        }
        self.set_notice(
            NoticeKind::Info,
            format!("已选择 {} 首歌曲", self.marked.len()),
        );
    }

    pub fn queue_downloads(&mut self) {
        let candidates: Vec<MarkedSong> = if self.marked.is_empty() {
            self.results
                .get(self.selected_row)
                .cloned()
                .map(|song| {
                    vec![MarkedSong {
                        platform: self.platform,
                        song,
                    }]
                })
                .unwrap_or_default()
        } else {
            self.marked.values().cloned().collect()
        };

        if candidates.is_empty() {
            self.set_notice(NoticeKind::Warning, "请先搜索并选择要下载的歌曲");
            return;
        }

        let existing: HashSet<String> =
            self.downloads.iter().map(|item| item.key.clone()).collect();
        let mut queued = 0usize;
        let mut skipped = 0usize;
        for selected in candidates {
            let key = song_key(selected.platform, &selected.song.id);
            if existing.contains(&key) || self.downloads.iter().any(|item| item.key == key) {
                skipped += 1;
                continue;
            }

            let job_id = self.next_job_id;
            self.next_job_id += 1;
            self.downloads.push(DownloadItem {
                job_id,
                key: key.clone(),
                platform: selected.platform,
                song: selected.song.clone(),
                requested_quality: self.quality.clone(),
                actual_quality: None,
                received: 0,
                total: None,
                state: DownloadState::Queued,
            });
            self.spawn_download(job_id, selected);
            queued += 1;
        }
        self.marked.clear();

        if queued == 0 {
            self.set_notice(NoticeKind::Info, "所选歌曲已在下载列表中");
        } else {
            let suffix = if skipped > 0 {
                format!(",跳过 {skipped} 首重复歌曲")
            } else {
                String::new()
            };
            self.set_notice(
                NoticeKind::Success,
                format!(
                    "已加入 {queued} 个下载任务(最多 {MAX_CONCURRENT_DOWNLOADS} 路并发){suffix}"
                ),
            );
        }
    }

    fn spawn_download(&self, job_id: u64, selected: MarkedSong) {
        let client = Arc::clone(&self.client);
        let semaphore = Arc::clone(&self.download_semaphore);
        let tx = self.tx.clone();
        let token = self.cookies.token_for(selected.platform);
        let media_cookie = self.cookies.cookie_header_for(selected.platform);
        let requested_quality = self.quality.clone();
        let directory = self.paths.download_dir.clone();

        tokio::spawn(async move {
            let Ok(_permit) = semaphore.acquire_owned().await else {
                let _ = tx.send(AppMessage::DownloadStage {
                    job_id,
                    state: DownloadState::Failed("下载调度器已关闭".to_owned()),
                });
                return;
            };

            let _ = tx.send(AppMessage::DownloadStage {
                job_id,
                state: DownloadState::Resolving,
            });
            let playback = match resolve_url(
                &client,
                &selected.song.id,
                selected.platform,
                requested_quality,
                token.as_ref(),
            )
            .await
            {
                Ok(result) if !result.url.trim().is_empty() => result,
                Ok(_) => {
                    let _ = tx.send(AppMessage::DownloadStage {
                        job_id,
                        state: DownloadState::Failed(
                            "平台未返回可用链接,可能需要会员,登录或更换音质".to_owned(),
                        ),
                    });
                    return;
                }
                Err(error) => {
                    let _ = tx.send(AppMessage::DownloadStage {
                        job_id,
                        state: DownloadState::Failed(format!("获取播放链接失败:{error}")),
                    });
                    return;
                }
            };

            let _ = tx.send(AppMessage::DownloadResolved {
                job_id,
                actual_quality: playback.level.clone(),
            });

            let _ = tx.send(AppMessage::DownloadStage {
                job_id,
                state: DownloadState::Downloading,
            });
            match download::download_song(
                job_id,
                &playback.url,
                &selected.song,
                media_cookie.as_deref(),
                &directory,
                &tx,
            )
            .await
            {
                Ok(path) => {
                    let _ = tx.send(AppMessage::DownloadStage {
                        job_id,
                        state: DownloadState::Completed(path),
                    });
                }
                Err(error) => {
                    let _ = tx.send(AppMessage::DownloadStage {
                        job_id,
                        state: DownloadState::Failed(error.to_string()),
                    });
                }
            }
        });
    }

    pub fn clear_finished_downloads(&mut self) {
        let before = self.downloads.len();
        self.downloads.retain(|item| {
            !matches!(
                item.state,
                DownloadState::Completed(_) | DownloadState::Failed(_)
            )
        });
        let removed = before - self.downloads.len();
        self.set_notice(
            NoticeKind::Info,
            format!("已清理 {removed} 条完成/失败记录"),
        );
    }

    pub fn request_quit(&mut self) {
        let active = self
            .downloads
            .iter()
            .filter(|item| {
                matches!(
                    item.state,
                    DownloadState::Queued | DownloadState::Resolving | DownloadState::Downloading
                )
            })
            .count();
        if active > 0 {
            self.set_notice(
                NoticeKind::Warning,
                format!("还有 {active} 个下载任务未完成;请等待完成,或按 Ctrl+C 强制退出"),
            );
        } else {
            self.should_quit = true;
        }
    }

    pub fn start_login(&mut self) {
        self.cancel_login();
        self.login_request_id += 1;
        let request_id = self.login_request_id;
        let platform = self.platform;
        let client = Arc::clone(&self.client);
        let tx = self.tx.clone();
        let (cancel_tx, mut cancel_rx) = oneshot::channel();
        self.login_cancel = Some(cancel_tx);
        self.login_overlay = Some(LoginOverlay {
            request_id,
            phase: LoginPhase::Creating,
            qr_lines: Vec::new(),
            detail: "正在向平台申请登录二维码...".to_owned(),
        });

        tokio::spawn(async move {
            let session = tokio::select! {
                _ = &mut cancel_rx => return,
                result = client.login().session().platform(platform).send() => match result {
                    Ok(session) => session,
                    Err(error) => {
                        let _ = tx.send(AppMessage::LoginPhase {
                            request_id,
                            phase: LoginPhase::Failed,
                            detail: format!("创建登录会话失败:{error}"),
                        });
                        return;
                    }
                }
            };

            let payload = session.qr_code().to_owned();
            if let Err(error) = validate_qr_payload(&payload) {
                let _ = tx.send(AppMessage::LoginPhase {
                    request_id,
                    phase: LoginPhase::Failed,
                    detail: error.to_string(),
                });
                return;
            }
            let _ = tx.send(AppMessage::LoginQr {
                request_id,
                payload,
            });

            loop {
                let status = tokio::select! {
                    _ = &mut cancel_rx => return,
                    result = session.status() => result,
                };
                match status {
                    Ok(LoginStatus::Success(token)) => {
                        let _ = tx.send(AppMessage::LoginSuccess { request_id, token });
                        return;
                    }
                    Ok(LoginStatus::QrCodeExpired) => {
                        let _ = tx.send(AppMessage::LoginPhase {
                            request_id,
                            phase: LoginPhase::Expired,
                            detail: "二维码已过期,按 Ctrl+L 重新生成".to_owned(),
                        });
                        return;
                    }
                    Ok(LoginStatus::WaitingScan) => {
                        let _ = tx.send(AppMessage::LoginPhase {
                            request_id,
                            phase: LoginPhase::WaitingScan,
                            detail: "等待扫码...".to_owned(),
                        });
                    }
                    Ok(LoginStatus::WaitingConfirm) => {
                        let _ = tx.send(AppMessage::LoginPhase {
                            request_id,
                            phase: LoginPhase::WaitingConfirm,
                            detail: "已扫码,请在手机上确认登录...".to_owned(),
                        });
                    }
                    Err(MusicClientError::TencentMqttLogin(error))
                        if platform == Platform::Tencent =>
                    {
                        let _ = tx.send(AppMessage::LoginPhase {
                            request_id,
                            phase: LoginPhase::WaitingScan,
                            detail: format!("QQ 登录连接波动,正在自动重连:{error}"),
                        });
                    }
                    Err(error) => {
                        let _ = tx.send(AppMessage::LoginPhase {
                            request_id,
                            phase: LoginPhase::Failed,
                            detail: format!("登录状态查询失败:{error}"),
                        });
                        return;
                    }
                }

                tokio::select! {
                    _ = &mut cancel_rx => return,
                    _ = tokio::time::sleep(std::time::Duration::from_secs(2)) => {}
                }
            }
        });
    }

    pub fn cancel_login(&mut self) {
        if let Some(cancel) = self.login_cancel.take() {
            let _ = cancel.send(());
        }
        self.login_overlay = None;
    }

    pub fn handle_message(&mut self, message: AppMessage) {
        match message {
            AppMessage::SearchFinished { request_id, result } => {
                if request_id != self.search_request_id {
                    return;
                }
                self.loading = false;
                match result {
                    Ok(result) => {
                        let count = result.songs.len();
                        self.results = result.songs;
                        self.more = result.more;
                        self.selected_row = 0;
                        if count == 0 {
                            self.set_notice(NoticeKind::Warning, "没有找到歌曲,请尝试其他关键词");
                        } else {
                            self.set_notice(
                                NoticeKind::Success,
                                format!("第 {} 页找到 {count} 首歌曲", self.page),
                            );
                        }
                    }
                    Err(error) => {
                        self.results.clear();
                        self.more = false;
                        self.set_notice(NoticeKind::Error, format!("搜索失败:{error}"));
                    }
                }
            }
            AppMessage::LoginQr {
                request_id,
                payload,
            } => {
                let Some(overlay) = self.login_overlay.as_mut() else {
                    return;
                };
                if request_id != overlay.request_id
                    || matches!(overlay.phase, LoginPhase::Failed | LoginPhase::Expired)
                {
                    return;
                }
                match prepare_qr(&payload) {
                    Ok(lines) => {
                        overlay.qr_lines = lines;
                        overlay.phase = LoginPhase::WaitingScan;
                        overlay.detail = "请扫描终端中的二维码登录".to_owned();
                    }
                    Err(error) => {
                        overlay.phase = LoginPhase::Failed;
                        overlay.detail = format!("二维码处理失败:{error}");
                        if let Some(cancel) = self.login_cancel.take() {
                            let _ = cancel.send(());
                        }
                    }
                }
            }
            AppMessage::LoginPhase {
                request_id,
                phase,
                detail,
            } => {
                let Some(overlay) = self.login_overlay.as_mut() else {
                    return;
                };
                if request_id != overlay.request_id
                    || matches!(overlay.phase, LoginPhase::Failed | LoginPhase::Expired)
                {
                    return;
                }
                overlay.phase = phase;
                overlay.detail = detail;
                if matches!(phase, LoginPhase::Expired | LoginPhase::Failed) {
                    self.login_cancel = None;
                }
            }
            AppMessage::LoginSuccess { request_id, token } => {
                if !self.login_overlay.as_ref().is_some_and(|overlay| {
                    overlay.request_id == request_id
                        && !matches!(overlay.phase, LoginPhase::Failed | LoginPhase::Expired)
                }) {
                    return;
                }
                let logged_in_platform = match &token {
                    LoginToken::Netease(_) => Platform::Netease,
                    LoginToken::Tencent(_) => Platform::Tencent,
                };
                // 登录成功后立即同步保存;后续请求均从该存储读取 Cookie.
                let save_result = self.cookies.set_and_save(token, &self.paths.cookie_file);
                self.cookie_revisions[platform_index(logged_in_platform)] += 1;
                self.cookie_refresh_failed
                    .retain(|platform| *platform != logged_in_platform);
                self.login_overlay = None;
                self.login_cancel = None;
                match save_result {
                    Ok(()) => self.set_notice(
                        NoticeKind::Success,
                        format!(
                            "{} 登录成功,登录信息已保存",
                            Self::platform_label(logged_in_platform)
                        ),
                    ),
                    Err(error) => self.set_notice(
                        NoticeKind::Warning,
                        format!("登录成功,但保存登录信息失败:{error}"),
                    ),
                }
            }
            AppMessage::CookieRefreshFinished {
                platform,
                cookie_revision,
                result,
            } => {
                self.cookie_refresh_pending = self.cookie_refresh_pending.saturating_sub(1);
                if cookie_revision != self.cookie_revisions[platform_index(platform)] {
                    return;
                }
                match result {
                    Ok(token) => {
                        self.cookie_revisions[platform_index(platform)] += 1;
                        self.cookie_refresh_failed
                            .retain(|failed| *failed != platform);
                        match self.cookies.set_and_save(token, &self.paths.cookie_file) {
                            Ok(()) => self.set_notice(
                                NoticeKind::Success,
                                format!(
                                    "{} Cookie 已自动更新并立即保存",
                                    Self::platform_label(platform)
                                ),
                            ),
                            Err(error) => self.set_notice(
                                NoticeKind::Warning,
                                format!(
                                    "{} Cookie 已更新,但写入 {} 失败:{error}",
                                    Self::platform_label(platform),
                                    self.paths.cookie_file.display()
                                ),
                            ),
                        }
                    }
                    Err(error) => {
                        if !self.cookie_refresh_failed.contains(&platform) {
                            self.cookie_refresh_failed.push(platform);
                        }
                        self.set_notice(
                            NoticeKind::Warning,
                            format!(
                                "{} Cookie 自动更新失败:{error};请按 Ctrl+L 重新登录",
                                Self::platform_label(platform)
                            ),
                        );
                    }
                }
            }
            AppMessage::DownloadStage { job_id, state } => {
                if let Some(item) = self.downloads.iter_mut().find(|item| item.job_id == job_id) {
                    let finished = matches!(
                        state,
                        DownloadState::Completed(_) | DownloadState::Failed(_)
                    );
                    let description = match &state {
                        DownloadState::Completed(path) => Some((
                            NoticeKind::Success,
                            format!("<{}> 下载完成:{}", item.song.name, path.display()),
                        )),
                        DownloadState::Failed(error) => Some((
                            NoticeKind::Error,
                            format!("<{}> 下载失败:{error}", item.song.name),
                        )),
                        _ => None,
                    };
                    item.state = state;
                    if finished && let Some((kind, text)) = description {
                        self.set_notice(kind, text);
                    }
                }
            }
            AppMessage::DownloadResolved {
                job_id,
                actual_quality,
            } => {
                let mut downgrade = None;
                if let Some(item) = self.downloads.iter_mut().find(|item| item.job_id == job_id) {
                    if actual_quality != item.requested_quality {
                        downgrade = Some(format!(
                            "<{}> 请求音质为 {},平台实际返回 {}",
                            item.song.name,
                            Self::quality_name(&item.requested_quality),
                            Self::quality_name(&actual_quality)
                        ));
                    }
                    item.actual_quality = Some(actual_quality);
                }
                if let Some(message) = downgrade {
                    self.set_notice(NoticeKind::Warning, message);
                }
            }
            AppMessage::DownloadProgress {
                job_id,
                received,
                total,
            } => {
                if let Some(item) = self.downloads.iter_mut().find(|item| item.job_id == job_id) {
                    item.received = received;
                    item.total = total;
                    item.state = DownloadState::Downloading;
                }
            }
        }
    }

    pub fn is_marked(&self, song: &Song) -> bool {
        self.marked.contains_key(&song_key(self.platform, &song.id))
    }
}

fn platform_index(platform: Platform) -> usize {
    match platform {
        Platform::Netease => 0,
        Platform::Tencent => 1,
    }
}

fn song_key(platform: Platform, id: &str) -> String {
    let prefix = match platform {
        Platform::Netease => 'N',
        Platform::Tencent => 'Q',
    };
    format!("{prefix}:{id}")
}

async fn search_songs(
    client: &MusicClient,
    keyword: &str,
    platform: Platform,
    offset: u64,
    token: Option<&LoginToken>,
) -> netease_qq_music_api::MusicClientResult<SearchSongResult> {
    let request = client
        .search()
        .song()
        .keyword(keyword)
        .platform(platform)
        .offset(offset)
        .limit(PAGE_SIZE);
    match token {
        Some(LoginToken::Netease(token)) => request.login(token).send().await,
        Some(LoginToken::Tencent(token)) => request.login(token).send().await,
        None => request.send().await,
    }
}

async fn resolve_url(
    client: &MusicClient,
    song_id: &str,
    platform: Platform,
    quality: SongQuality,
    token: Option<&LoginToken>,
) -> netease_qq_music_api::MusicClientResult<UrlResult> {
    let request = client
        .playback()
        .url()
        .id(song_id)
        .platform(platform)
        .level(quality);
    let result = match token {
        Some(LoginToken::Netease(token)) => request.login(token).send().await?,
        Some(LoginToken::Tencent(token)) => request.login(token).send().await?,
        None => request.send().await?,
    };
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use netease_qq_music_api::models::{NeteaseLoginToken, TencentLoginToken};

    fn test_app() -> App {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = App::new(
            Arc::new(MusicClient::new()),
            tx,
            AppPaths {
                cookie_file: "__dqn_app_test_cookie.json".into(),
                download_dir: "test-downloads".into(),
            },
        );
        // Exercise in-memory login even if saving fails, without creating a cookie file.
        app.paths.cookie_file = std::env::current_dir().unwrap();
        app
    }

    fn token(platform: Platform, key: &str) -> LoginToken {
        match platform {
            Platform::Netease => {
                LoginToken::Netease(NeteaseLoginToken::new(key, key, key, Some(i64::MAX)))
            }
            Platform::Tencent => {
                LoginToken::Tencent(TencentLoginToken::new(1, key, key, key, Some(i64::MAX), 1))
            }
        }
    }

    fn complete_login(app: &mut App, platform: Platform) {
        app.login_overlay = Some(LoginOverlay {
            request_id: 1,
            phase: LoginPhase::WaitingConfirm,
            qr_lines: Vec::new(),
            detail: String::new(),
        });
        app.handle_message(AppMessage::LoginSuccess {
            request_id: 1,
            token: token(platform, "new"),
        });
    }

    #[test]
    fn switching_platform_discards_in_flight_search_when_input_is_empty() {
        for platform in [Platform::Netease, Platform::Tencent] {
            for result in [
                Ok(SearchSongResult {
                    songs: Vec::new(),
                    more: true,
                }),
                Err("old platform request failed".into()),
            ] {
                let mut app = test_app();
                app.platform = platform;
                app.query = "previous query".into();
                app.input.clear();
                app.search_request_id = 7;
                app.loading = true;

                app.toggle_platform();
                let notice = app.notice.text.clone();
                assert_ne!(app.platform, platform);
                assert!(!app.loading);

                app.handle_message(AppMessage::SearchFinished {
                    request_id: 7,
                    result,
                });
                assert!(app.results.is_empty());
                assert!(!app.more);
                assert!(!app.loading);
                assert_eq!(app.notice.text, notice);
            }
        }
    }

    #[test]
    fn invalid_qr_cancels_polling_and_keeps_failure_until_a_new_login() {
        let mut app = test_app();
        let (cancel_tx, mut cancel_rx) = oneshot::channel();
        app.login_cancel = Some(cancel_tx);
        app.login_overlay = Some(LoginOverlay {
            request_id: 1,
            phase: LoginPhase::Creating,
            qr_lines: Vec::new(),
            detail: String::new(),
        });
        app.handle_message(AppMessage::LoginQr {
            request_id: 1,
            payload: "data:image/png;base64,!!!!".into(),
        });
        assert_eq!(cancel_rx.try_recv(), Ok(()));
        let failure = app.login_overlay.as_ref().unwrap().detail.clone();

        // Messages already queued before cancellation must not revive the failed session.
        for phase in [
            LoginPhase::WaitingScan,
            LoginPhase::WaitingConfirm,
            LoginPhase::Expired,
        ] {
            app.handle_message(AppMessage::LoginPhase {
                request_id: 1,
                phase,
                detail: "late polling response".into(),
            });
        }
        app.handle_message(AppMessage::LoginSuccess {
            request_id: 1,
            token: token(Platform::Netease, "late"),
        });
        let overlay = app.login_overlay.as_ref().unwrap();
        assert_eq!(overlay.phase, LoginPhase::Failed);
        assert_eq!(overlay.detail, failure);
        assert!(app.cookies.token_for(Platform::Netease).is_none());

        app.login_overlay = Some(LoginOverlay {
            request_id: 2,
            phase: LoginPhase::Creating,
            qr_lines: Vec::new(),
            detail: String::new(),
        });
        app.handle_message(AppMessage::LoginQr {
            request_id: 2,
            payload: "https://example.com/login".into(),
        });
        let overlay = app.login_overlay.as_ref().unwrap();
        assert_eq!(overlay.phase, LoginPhase::WaitingScan);
        assert!(!overlay.qr_lines.is_empty());
    }

    #[test]
    fn stale_refresh_cannot_replace_new_login_or_raise_a_false_warning() {
        for platform in [Platform::Netease, Platform::Tencent] {
            for result in [Ok(token(platform, "old")), Err("old request failed".into())] {
                let mut app = test_app();
                app.cookies.set(token(platform, "old"));
                app.cookie_refresh_pending = 1;
                complete_login(&mut app, platform);
                let new_cookie = app.cookies.cookie_header_for(platform);
                let notice = app.notice.text.clone();

                app.handle_message(AppMessage::CookieRefreshFinished {
                    platform,
                    cookie_revision: 0,
                    result,
                });

                assert_eq!(app.cookies.cookie_header_for(platform), new_cookie);
                assert_eq!(app.notice.text, notice);
                assert!(app.cookie_warnings().is_empty());
                assert_eq!(app.cookie_refresh_pending, 0);
            }
        }
    }

    #[test]
    fn login_on_one_platform_does_not_discard_other_platform_refresh() {
        let mut app = test_app();
        app.cookie_refresh_pending = 1;
        complete_login(&mut app, Platform::Netease);
        app.handle_message(AppMessage::CookieRefreshFinished {
            platform: Platform::Tencent,
            cookie_revision: 0,
            result: Ok(token(Platform::Tencent, "refreshed")),
        });
        assert!(
            app.cookies
                .cookie_header_for(Platform::Netease)
                .unwrap()
                .contains("new")
        );
        assert!(
            app.cookies
                .cookie_header_for(Platform::Tencent)
                .unwrap()
                .contains("refreshed")
        );
        assert_eq!(app.cookie_refresh_pending, 0);
    }
}
