use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use netease_qq_music_api::models::{LoginToken, NeteaseLoginToken, Platform, TencentLoginToken};
use serde::{Deserialize, Serialize};

const COOKIE_FILE_NAME: &str = "music_cookie.json";

#[derive(Clone, Debug)]
pub struct AppPaths {
    pub cookie_file: PathBuf,
    pub download_dir: PathBuf,
}

impl AppPaths {
    /// 将相对路径基准固定到可执行文件目录,并迁移旧启动目录中的 Cookie.
    pub fn enter_app_directory() -> Result<()> {
        let launch_dir = std::env::current_dir().context("无法确定启动工作目录")?;
        let executable = std::env::current_exe().context("无法确定应用程序路径")?;
        let app_dir = executable.parent().context("应用程序路径没有父目录")?;
        if launch_dir == app_dir {
            return Ok(());
        }

        let old_cookie = launch_dir.join(COOKIE_FILE_NAME);
        let app_cookie = app_dir.join(COOKIE_FILE_NAME);
        if old_cookie.is_file()
            && !app_cookie.exists()
            && fs::rename(&old_cookie, &app_cookie).is_err()
        {
            fs::copy(&old_cookie, &app_cookie).with_context(|| {
                format!(
                    "无法将登录信息从 {} 迁移到 {}",
                    old_cookie.display(),
                    app_cookie.display()
                )
            })?;
            if let Err(error) = fs::remove_file(&old_cookie) {
                let _ = fs::remove_file(&app_cookie);
                return Err(error)
                    .with_context(|| format!("无法清理旧登录信息:{}", old_cookie.display()));
            }
        }
        std::env::set_current_dir(app_dir)
            .with_context(|| format!("无法进入应用程序目录:{}", app_dir.display()))
    }

    pub fn discover() -> Result<Self> {
        let executable = std::env::current_exe().context("无法确定应用程序路径")?;
        let app_dir = executable
            .parent()
            .context("应用程序路径没有父目录")?
            .to_path_buf();

        Ok(Self {
            cookie_file: PathBuf::from(COOKIE_FILE_NAME),
            download_dir: app_dir.join("downloads"),
        })
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CookieStore {
    #[serde(default = "cookie_version")]
    version: u8,
    #[serde(default)]
    netease: Option<NeteaseLoginToken>,
    #[serde(default)]
    tencent: Option<TencentLoginToken>,
}

const fn cookie_version() -> u8 {
    1
}

impl Default for CookieStore {
    fn default() -> Self {
        Self {
            version: cookie_version(),
            netease: None,
            tencent: None,
        }
    }
}

impl CookieStore {
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }

        let json = fs::read_to_string(path)
            .with_context(|| format!("无法读取登录信息:{}", path.display()))?;
        serde_json::from_str(&json)
            .with_context(|| format!("登录信息 JSON 格式无效:{}", path.display()))
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        let json = serde_json::to_string_pretty(self).context("无法序列化登录信息")?;
        fs::write(path, json).with_context(|| format!("无法保存登录信息:{}", path.display()))
    }

    pub fn set(&mut self, token: LoginToken) {
        match token {
            LoginToken::Netease(token) => self.netease = Some(token),
            LoginToken::Tencent(token) => self.tencent = Some(token),
        }
    }

    /// 更新内存中的登录信息并立即写入指定 JSON 文件.
    pub fn set_and_save(&mut self, token: LoginToken, path: &Path) -> Result<()> {
        self.set(token);
        self.save(path)
    }

    pub fn token_for(&self, platform: Platform) -> Option<LoginToken> {
        self.stored_token_for(platform)
            .filter(|token| !token_is_expired_at(token, unix_now()))
    }

    fn stored_token_for(&self, platform: Platform) -> Option<LoginToken> {
        match platform {
            Platform::Netease => self.netease.clone().map(LoginToken::Netease),
            Platform::Tencent => self.tencent.clone().map(LoginToken::Tencent),
        }
    }

    /// 返回可用于实际媒体下载请求的 Cookie 请求头.
    pub fn cookie_header_for(&self, platform: Platform) -> Option<String> {
        match self.token_for(platform)? {
            LoginToken::Netease(token) => Some(token.to_cookie()),
            LoginToken::Tencent(token) => Some(token.to_cookie()),
        }
    }

    /// 返回启动时需要刷新的 Token.过期时间未知时也刷新一次,以获取最新有效期.
    pub fn tokens_requiring_refresh(
        &self,
        refresh_before: Duration,
    ) -> Vec<(Platform, LoginToken)> {
        self.tokens_requiring_refresh_at(unix_now(), refresh_before.as_secs() as i64)
    }

    fn tokens_requiring_refresh_at(
        &self,
        now: i64,
        refresh_before_seconds: i64,
    ) -> Vec<(Platform, LoginToken)> {
        let refresh_at = now.saturating_add(refresh_before_seconds);
        [Platform::Netease, Platform::Tencent]
            .into_iter()
            .filter_map(|platform| {
                let token = self.stored_token_for(platform)?;
                token_expires_at(&token)
                    .is_none_or(|expires_at| expires_at <= refresh_at)
                    .then_some((platform, token))
            })
            .collect()
    }

    pub fn stored_count(&self) -> usize {
        usize::from(self.netease.is_some()) + usize::from(self.tencent.is_some())
    }

    pub fn login_state(&self, platform: Platform) -> &'static str {
        match self.stored_token_for(platform) {
            None => "未登录",
            Some(token) if token_is_expired_at(&token, unix_now()) => "登录已过期",
            Some(_) => "已登录",
        }
    }
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or_default()
}

fn token_expires_at(token: &LoginToken) -> Option<i64> {
    match token {
        LoginToken::Netease(token) => token.expires_at(),
        LoginToken::Tencent(token) => token.expires_at(),
    }
}

fn token_is_expired_at(token: &LoginToken, now: i64) -> bool {
    token_expires_at(token).is_some_and(|expires_at| now >= expires_at)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cookie_path_is_relative() {
        let paths = AppPaths::discover().unwrap();
        assert!(paths.cookie_file.is_relative());
        assert_eq!(paths.cookie_file, PathBuf::from("music_cookie.json"));
    }

    #[test]
    fn cookie_store_keeps_tokens_for_both_platforms() {
        let mut store = CookieStore::default();
        store.set(LoginToken::Netease(NeteaseLoginToken::new(
            "u", "ru", "csrf", None,
        )));
        store.set(LoginToken::Tencent(TencentLoginToken::new(
            42,
            "key",
            "refresh",
            "refresh-key",
            None,
            1,
        )));

        let json = serde_json::to_string(&store).unwrap();
        let decoded: CookieStore = serde_json::from_str(&json).unwrap();
        assert!(matches!(
            decoded.token_for(Platform::Netease),
            Some(LoginToken::Netease(_))
        ));
        assert!(matches!(
            decoded.token_for(Platform::Tencent),
            Some(LoginToken::Tencent(_))
        ));
        assert_eq!(
            decoded.cookie_header_for(Platform::Netease).as_deref(),
            Some("MUSIC_U=u; __csrf=csrf")
        );
        assert_eq!(
            decoded.cookie_header_for(Platform::Tencent).as_deref(),
            Some("uin=42; qqmusic_key=key; qm_keyst=key; tmeLoginType=1")
        );
    }

    #[test]
    fn login_token_is_saved_immediately() {
        let path =
            std::env::temp_dir().join(format!("dqn-cookie-test-{}.json", std::process::id()));
        let _ = fs::remove_file(&path);

        let mut store = CookieStore::default();
        store
            .set_and_save(
                LoginToken::Netease(NeteaseLoginToken::new("u", "ru", "csrf", None)),
                &path,
            )
            .unwrap();

        let loaded = CookieStore::load(&path).unwrap();
        assert!(loaded.token_for(Platform::Netease).is_some());
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn expired_token_is_not_used_and_is_scheduled_for_refresh() {
        let now = 1_000_000;
        let mut store = CookieStore::default();
        store.set(LoginToken::Netease(NeteaseLoginToken::new(
            "u",
            "ru",
            "csrf",
            Some(now - 1),
        )));

        assert!(token_is_expired_at(
            &store.stored_token_for(Platform::Netease).unwrap(),
            now
        ));
        assert_eq!(
            store.tokens_requiring_refresh_at(now, 24 * 60 * 60)[0].0,
            Platform::Netease
        );
    }

    #[test]
    fn near_expiry_and_unknown_expiry_are_refreshed() {
        let now = 1_000_000;
        let mut store = CookieStore::default();
        store.set(LoginToken::Netease(NeteaseLoginToken::new(
            "u",
            "ru",
            "csrf",
            Some(now + 60 * 60),
        )));
        store.set(LoginToken::Tencent(TencentLoginToken::new(
            42,
            "key",
            "refresh",
            "refresh-key",
            None,
            1,
        )));

        let candidates = store.tokens_requiring_refresh_at(now, 24 * 60 * 60);
        assert_eq!(candidates.len(), 2);
        assert_eq!(candidates[0].0, Platform::Netease);
        assert_eq!(candidates[1].0, Platform::Tencent);
    }
}
