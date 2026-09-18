use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use futures_util::StreamExt;
use netease_qq_music_api::models::Song;
use reqwest::header::COOKIE;
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;

use crate::app::AppMessage;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const READ_TIMEOUT: Duration = Duration::from_secs(30);

fn download_client_builder(read_timeout: Duration) -> reqwest::ClientBuilder {
    reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .read_timeout(read_timeout)
}

pub async fn download_song(
    job_id: u64,
    url: &str,
    song: &Song,
    cookie_header: Option<&str>,
    directory: &Path,
    tx: &mpsc::UnboundedSender<AppMessage>,
) -> Result<PathBuf> {
    tokio::fs::create_dir_all(directory)
        .await
        .with_context(|| format!("无法创建下载目录:{}", directory.display()))?;

    let extension = original_url_extension(url)?;
    let client = download_client_builder(READ_TIMEOUT)
        .build()
        .context("无法初始化下载客户端")?;
    let mut request = client.get(url);
    if let Some(cookie) = cookie_header.filter(|cookie| !cookie.is_empty()) {
        request = request.header(COOKIE, cookie);
    }
    let response = request
        .send()
        .await
        .map_err(|error| {
            let message = if error.is_timeout() {
                "连接下载地址或等待响应超时,请重试"
            } else {
                "连接下载地址失败"
            };
            anyhow::Error::new(error).context(message)
        })?
        .error_for_status()
        .context("下载服务器返回错误状态")?;
    save_response(job_id, response, song, &extension, directory, tx).await
}

async fn save_response(
    job_id: u64,
    response: reqwest::Response,
    song: &Song,
    extension: &str,
    directory: &Path,
    tx: &mpsc::UnboundedSender<AppMessage>,
) -> Result<PathBuf> {
    let total = response.content_length();
    let mut stream = response.bytes_stream();
    let stem = file_stem(song);
    let (final_path, partial_path, mut file) =
        reserve_download_file(directory, &stem, extension).await?;

    let _ = tx.send(AppMessage::DownloadProgress {
        job_id,
        received: 0,
        total,
    });
    let transfer_result: Result<()> = async {
        let mut received = 0u64;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|error| {
                let message = if error.is_timeout() {
                    "下载读取超时,请重试"
                } else {
                    "读取下载数据失败"
                };
                anyhow::Error::new(error).context(message)
            })?;
            file.write_all(&chunk).await.context("写入下载文件失败")?;
            received += chunk.len() as u64;
            let _ = tx.send(AppMessage::DownloadProgress {
                job_id,
                received,
                total,
            });
        }
        if received == 0 {
            bail!("下载服务器返回了空文件");
        }
        file.flush().await.context("刷新下载文件失败")?;
        Ok(())
    }
    .await;

    if let Err(error) = transfer_result {
        drop(file);
        let _ = tokio::fs::remove_file(&partial_path).await;
        return Err(error);
    }
    drop(file);
    if let Err(error) = tokio::fs::rename(&partial_path, &final_path).await {
        let _ = tokio::fs::remove_file(&partial_path).await;
        return Err(error).with_context(|| format!("无法完成下载文件:{}", final_path.display()));
    }
    Ok(final_path)
}

async fn reserve_download_file(
    directory: &Path,
    stem: &str,
    extension: &str,
) -> Result<(PathBuf, PathBuf, tokio::fs::File)> {
    for sequence in 0..10_000u32 {
        let suffix = if sequence == 0 {
            String::new()
        } else {
            format!(" ({sequence})")
        };
        let final_path = directory.join(format!("{stem}{suffix}.{extension}"));
        let partial_path = directory.join(format!("{stem}{suffix}.{extension}.part"));
        if final_path.exists() {
            continue;
        }

        match tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&partial_path)
            .await
        {
            Ok(file) => return Ok((final_path, partial_path, file)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("无法创建临时下载文件:{}", partial_path.display()));
            }
        }
    }
    bail!("同名下载文件过多,请整理下载目录后重试")
}

fn original_url_extension(url: &str) -> Result<String> {
    let parsed = reqwest::Url::parse(url).context("平台返回的下载链接格式无效")?;
    let file_name = parsed
        .path_segments()
        .and_then(Iterator::last)
        .filter(|name| !name.is_empty())
        .context("平台返回的下载链接没有原始文件名")?;
    let extension = file_name
        .rsplit_once('.')
        .map(|(_, extension)| extension)
        .filter(|extension| {
            !extension.is_empty()
                && extension.len() <= 16
                && extension
                    .chars()
                    .all(|character| character.is_ascii_alphanumeric())
        })
        .context("平台返回的下载链接没有可用的原始扩展名")?;
    Ok(extension.to_owned())
}

fn file_stem(song: &Song) -> String {
    let artists = if song.artists.is_empty() {
        "未知歌手".to_owned()
    } else {
        song.artists
            .iter()
            .map(|artist| artist.name.as_str())
            .collect::<Vec<_>>()
            .join(",")
    };
    sanitize_filename(&format!("{} - {artists}", song.name))
}

pub fn sanitize_filename(value: &str) -> String {
    let mut sanitized = String::with_capacity(value.len());
    let mut previous_space = false;
    for character in value.chars() {
        let replacement = if character.is_control()
            || matches!(
                character,
                '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*'
            ) {
            '_'
        } else {
            character
        };
        if replacement.is_whitespace() {
            if previous_space {
                continue;
            }
            sanitized.push(' ');
            previous_space = true;
        } else {
            sanitized.push(replacement);
            previous_space = false;
        }
        if sanitized.chars().count() >= 140 {
            break;
        }
    }

    let sanitized = sanitized.trim().trim_end_matches(['.', ' ']).to_owned();
    if sanitized.is_empty() {
        "未命名歌曲".to_owned()
    } else {
        sanitized
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use netease_qq_music_api::models::{SongAlbum, SongArtist};
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct TestServer {
        url: String,
        release: std::sync::mpsc::Sender<()>,
        thread: Option<std::thread::JoinHandle<()>>,
    }

    impl TestServer {
        fn new(response: &'static [u8]) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let url = format!("http://{}/song.mp3", listener.local_addr().unwrap());
            let (release, wait) = std::sync::mpsc::channel();
            let thread = std::thread::spawn(move || {
                let (mut socket, _) = listener.accept().unwrap();
                socket
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut request = Vec::new();
                let mut buffer = [0; 1024];
                while !request.windows(4).any(|part| part == b"\r\n\r\n") {
                    let count = socket.read(&mut buffer).unwrap();
                    if count == 0 {
                        return;
                    }
                    request.extend_from_slice(&buffer[..count]);
                }
                socket.write_all(response).unwrap();
                socket.flush().unwrap();
                // Keep the connection open without sending the rest of the response.
                let _ = wait.recv_timeout(Duration::from_secs(5));
            });
            Self {
                url,
                release,
                thread: Some(thread),
            }
        }
    }

    impl Drop for TestServer {
        fn drop(&mut self) {
            let _ = self.release.send(());
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }

    fn test_client() -> reqwest::Client {
        download_client_builder(Duration::from_millis(250))
            .no_proxy()
            .build()
            .unwrap()
    }

    fn test_song() -> Song {
        Song {
            id: "test".into(),
            name: "test".into(),
            pic_url: String::new(),
            artists: Vec::new(),
            album: SongAlbum {
                id: "test".into(),
                name: "test".into(),
            },
        }
    }

    fn test_directory() -> PathBuf {
        static NEXT_ID: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "dqn-download-test-{}-{}",
            std::process::id(),
            NEXT_ID.fetch_add(1, Ordering::Relaxed),
        ));
        std::fs::create_dir(&path).unwrap();
        path
    }

    #[tokio::test]
    async fn stalled_response_headers_time_out() {
        let server = TestServer::new(b"");
        let error = test_client().get(&server.url).send().await.unwrap_err();
        assert!(error.is_timeout());
    }

    #[tokio::test]
    async fn stalled_download_times_out_and_removes_partial_file() {
        let server = TestServer::new(b"HTTP/1.1 200 OK\r\nContent-Length: 6\r\n\r\nab");
        let response = test_client().get(&server.url).send().await.unwrap();
        let directory = test_directory();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let error = save_response(1, response, &test_song(), "mp3", &directory, &tx)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("超时"));
        assert!(error.downcast_ref::<reqwest::Error>().unwrap().is_timeout());
        let mut received_data = false;
        while let Ok(message) = rx.try_recv() {
            if matches!(message, AppMessage::DownloadProgress { received: 2, .. }) {
                received_data = true;
            }
        }
        assert!(
            received_data,
            "timeout must occur after the partial file was written"
        );
        assert_eq!(std::fs::read_dir(&directory).unwrap().count(), 0);
        std::fs::remove_dir(directory).unwrap();
    }

    #[tokio::test]
    async fn complete_download_still_renames_partial_file() {
        let server = TestServer::new(b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\ntest");
        let response = test_client().get(&server.url).send().await.unwrap();
        let directory = test_directory();
        let (tx, _rx) = mpsc::unbounded_channel();
        let path = save_response(1, response, &test_song(), "mp3", &directory, &tx)
            .await
            .unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"test");
        assert_eq!(path.extension().unwrap(), "mp3");
        assert_eq!(std::fs::read_dir(&directory).unwrap().count(), 1);
        std::fs::remove_file(path).unwrap();
        std::fs::remove_dir(directory).unwrap();
    }

    #[test]
    fn filename_is_safe_on_windows() {
        assert_eq!(sanitize_filename(" A:B/C*D?  "), "A_B_C_D_");
    }

    #[test]
    fn filename_contains_only_song_name_and_artists() {
        let song = Song {
            id: "123456".to_owned(),
            name: "歌曲名".to_owned(),
            pic_url: String::new(),
            artists: vec![
                SongArtist {
                    id: "1".to_owned(),
                    name: "歌手甲".to_owned(),
                },
                SongArtist {
                    id: "2".to_owned(),
                    name: "歌手乙".to_owned(),
                },
            ],
            album: SongAlbum {
                id: "3".to_owned(),
                name: "专辑".to_owned(),
            },
        };

        assert_eq!(file_stem(&song), "歌曲名 - 歌手甲,歌手乙");
        assert!(!file_stem(&song).contains(&song.id));
    }

    #[test]
    fn extension_is_kept_from_original_url() {
        assert_eq!(
            original_url_extension("https://x/path/song.mp3?token=1").unwrap(),
            "mp3"
        );
        assert_eq!(
            original_url_extension("https://x/path/song.FLAC#play").unwrap(),
            "FLAC"
        );
    }

    #[test]
    fn missing_original_extension_is_not_guessed() {
        assert!(original_url_extension("https://x/download?id=1").is_err());
    }
}
