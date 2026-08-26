use std::io::Cursor;

use anyhow::{Context, Result, anyhow};
use base64::Engine as _;
use image::{DynamicImage, GrayImage, ImageFormat, Luma};
use qrcode::QrCode;

const QUIET_ZONE_MODULES: usize = 4;

pub fn prepare_qr(payload: &str) -> Result<Vec<String>> {
    let png = payload_to_png(payload)?;
    let image = image::load_from_memory(&png)
        .context("平台返回的二维码不是有效图片")?
        .to_luma8();
    let content = decode_qr_content(&image)?;
    let canonical = QrCode::new(content.as_bytes())
        .context("无法重新生成标准终端二维码")?
        .render::<Luma<u8>>()
        .quiet_zone(false)
        .module_dimensions(1, 1)
        .build();
    Ok(render_terminal_qr(&canonical))
}

fn decode_qr_content(image: &GrayImage) -> Result<String> {
    let mut prepared = rqrr::PreparedImage::prepare(image.clone());
    prepared
        .detect_grids()
        .into_iter()
        .find_map(|grid| grid.decode().ok().map(|(_, content)| content))
        .context("无法识别平台返回的二维码内容")
}

fn payload_to_png(payload: &str) -> Result<Vec<u8>> {
    if let Some((metadata, encoded)) = payload.split_once(',')
        && metadata.starts_with("data:image/")
        && metadata.ends_with(";base64")
    {
        return base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .context("无法解码二维码 Base64 数据");
    }

    if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(payload)
        && image::load_from_memory(&bytes).is_ok()
    {
        return Ok(bytes);
    }

    let code = QrCode::new(payload.as_bytes()).context("无法生成登录二维码")?;
    let image = code
        .render::<Luma<u8>>()
        .quiet_zone(true)
        .module_dimensions(8, 8)
        .build();
    let mut png = Vec::new();
    DynamicImage::ImageLuma8(image)
        .write_to(&mut Cursor::new(&mut png), ImageFormat::Png)
        .context("无法编码登录二维码")?;
    Ok(png)
}

fn render_terminal_qr(image: &GrayImage) -> Vec<String> {
    let (width, height) = image.dimensions();
    if width == 0 || height == 0 {
        return vec!["平台返回的二维码图片为空".to_owned()];
    }

    let Some((pixel_left, pixel_top, pixel_right, pixel_bottom)) = dark_pixel_bounds(image) else {
        return vec!["平台返回的二维码图片为空".to_owned()];
    };
    let unit = detect_module_size(image);
    let columns = (pixel_right - pixel_left + 1).div_ceil(unit).max(1);
    let rows = (pixel_bottom - pixel_top + 1).div_ceil(unit).max(1);
    let mut dark = vec![vec![false; columns as usize]; rows as usize];

    for row in 0..rows {
        for column in 0..columns {
            let x = (pixel_left + column * unit + unit / 2).min(pixel_right);
            let y = (pixel_top + row * unit + unit / 2).min(pixel_bottom);
            dark[row as usize][column as usize] = image.get_pixel(x, y).0[0] < 128;
        }
    }

    let padded_width = columns as usize + QUIET_ZONE_MODULES * 2;
    let padded_height = rows as usize + QUIET_ZONE_MODULES * 2;
    let module_at = |row: usize, column: usize| {
        row.checked_sub(QUIET_ZONE_MODULES)
            .zip(column.checked_sub(QUIET_ZONE_MODULES))
            .and_then(|(row, column)| dark.get(row).and_then(|line| line.get(column)))
            .copied()
            .unwrap_or(false)
    };
    let mut lines = Vec::with_capacity(padded_height.div_ceil(2));
    for upper_row in (0..padded_height).step_by(2) {
        let mut line = String::with_capacity(padded_width);
        for column in 0..padded_width {
            let upper = module_at(upper_row, column);
            let lower = upper_row + 1 < padded_height && module_at(upper_row + 1, column);
            line.push(match (upper, lower) {
                (true, true) => '█',
                (true, false) => '▀',
                (false, true) => '▄',
                (false, false) => ' ',
            });
        }
        lines.push(line);
    }
    lines
}

fn dark_pixel_bounds(image: &GrayImage) -> Option<(u32, u32, u32, u32)> {
    let mut left = u32::MAX;
    let mut top = u32::MAX;
    let mut right = 0u32;
    let mut bottom = 0u32;
    let mut found = false;

    for (x, y, pixel) in image.enumerate_pixels() {
        if pixel.0[0] < 128 {
            found = true;
            left = left.min(x);
            top = top.min(y);
            right = right.max(x);
            bottom = bottom.max(y);
        }
    }
    found.then_some((left, top, right, bottom))
}

fn detect_module_size(image: &GrayImage) -> u32 {
    let (width, height) = image.dimensions();
    let mut dark_runs = Vec::new();
    for y in 0..height {
        let mut run = 0u32;
        for x in 0..width {
            if image.get_pixel(x, y).0[0] < 128 {
                run += 1;
            } else if run > 0 {
                dark_runs.push(run);
                run = 0;
            }
        }
        if run > 0 {
            dark_runs.push(run);
        }
    }
    for x in 0..width {
        let mut run = 0u32;
        for y in 0..height {
            if image.get_pixel(x, y).0[0] < 128 {
                run += 1;
            } else if run > 0 {
                dark_runs.push(run);
                run = 0;
            }
        }
        if run > 0 {
            dark_runs.push(run);
        }
    }

    let max_candidate = width.min(height).min(64);
    (2..=max_candidate)
        .rev()
        .find(|candidate| {
            let matching = dark_runs.iter().filter(|run| *run % candidate == 0).count();
            !dark_runs.is_empty() && matching * 100 >= dark_runs.len() * 90
        })
        .unwrap_or(1)
}

pub fn validate_qr_payload(payload: &str) -> Result<()> {
    if payload.trim().is_empty() {
        return Err(anyhow!("平台返回了空二维码"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_qr_is_rendered_at_module_resolution() {
        let png = payload_to_png("https://example.com/login?id=123").unwrap();
        let image = image::load_from_memory(&png).unwrap().to_luma8();
        let lines = render_terminal_qr(&image);
        assert!(lines.len() >= 10);
        assert!(lines.iter().any(|line| line.contains('█')));
        assert!(
            lines
                .iter()
                .all(|line| line.chars().count() == lines[0].chars().count())
        );
        assert!(lines[..2].iter().all(|line| line.trim().is_empty()));
        assert!(
            lines[lines.len() - 2..]
                .iter()
                .all(|line| line.trim().is_empty())
        );
    }

    #[test]
    fn arbitrary_image_padding_keeps_qq_qr_modules_aligned() {
        let code = QrCode::new(b"https://y.qq.com/login/test").unwrap();
        let image = code
            .render::<Luma<u8>>()
            .quiet_zone(false)
            .module_dimensions(6, 6)
            .build();
        let mut padded =
            GrayImage::from_pixel(image.width() + 37, image.height() + 29, Luma([255]));
        image::imageops::replace(&mut padded, &image, 17, 11);

        assert_eq!(detect_module_size(&padded), 6);
        let lines = render_terminal_qr(&padded);
        assert_eq!(
            lines[0].chars().count(),
            image.width() as usize / 6 + QUIET_ZONE_MODULES * 2
        );
        assert!(lines.iter().any(|line| line.contains('█')));
    }

    #[test]
    fn decorated_image_is_decoded_and_reencoded_to_compact_terminal_qr() {
        let code = QrCode::new(b"https://y.qq.com/login/test").unwrap();
        let image = code
            .render::<Luma<u8>>()
            .quiet_zone(true)
            .module_dimensions(6, 6)
            .build();
        let mut png = Vec::new();
        DynamicImage::ImageLuma8(image)
            .write_to(&mut Cursor::new(&mut png), ImageFormat::Png)
            .unwrap();
        let payload = format!(
            "data:image/png;base64,{}",
            base64::engine::general_purpose::STANDARD.encode(png)
        );

        let lines = prepare_qr(&payload).unwrap();
        assert!(lines[0].chars().count() < 60);
        assert!(lines.len() < 30);
    }
}
