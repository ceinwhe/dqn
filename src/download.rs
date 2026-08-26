use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use futures_util::StreamExt;
use netease_qq_music_api::models::Song;
use reqwest::header::COOKIE;
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;

use crate::app::AppMessage;

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
    let client = reqwest::Client::new();
    let mut request = client.get(url);
    if let Some(cookie) = cookie_header.filter(|cookie| !cookie.is_empty()) {
        request = request.header(COOKIE, cookie);
    }
    let response = request
        .send()
        .await
        .context("连接下载地址失败")?
        .error_for_status()
        .context("下载服务器返回错误状态")?;
    let total = response.content_length();
    let mut stream = response.bytes_stream();
    let stem = file_stem(song);
    let (final_path, partial_path, mut file) =
        reserve_download_file(directory, &stem, &extension).await?;

    let _ = tx.send(AppMessage::DownloadProgress {
        job_id,
        received: 0,
        total,
    });
    let transfer_result: Result<()> = async {
        let mut received = 0u64;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.context("读取下载数据失败")?;
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
