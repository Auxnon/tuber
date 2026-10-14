//! Cover art for the player, drawn with half blocks (`▀`, top pixel in the
//! foreground and bottom pixel in the background) behind the visualizer.

use anyhow::{Context, Result};
use image::imageops::{self, FilterType};
use image::{Rgb, RgbImage};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Color;
use std::io::Read;

/// Stored size; drawing scales from this.
const SIZE: u32 = 128;
/// Brightness of the drawn cover, so the visualizer stays readable on top.
const DIM: f32 = 0.42;

/// Download, trim, crop square and shrink a cover.
pub fn fetch(url: &str) -> Result<RgbImage> {
    // googleusercontent art takes its size in the URL; ask for a small one.
    let url = match url.rfind('=') {
        Some(i) if url.contains("googleusercontent.com") => format!("{}=w{SIZE}-h{SIZE}-l90-rj", &url[..i]),
        _ => url.to_string(),
    };
    let mut bytes = Vec::new();
    ureq::get(&url)
        .call()
        .context("downloading cover")?
        .into_body()
        .into_reader()
        .take(4 << 20)
        .read_to_end(&mut bytes)?;
    let img = image::load_from_memory(&bytes).context("decoding cover")?.to_rgb8();
    Ok(square(trim(img)))
}

/// Cut black letterbox bars, which video thumbnails (4:3 frames around
/// 16:9 pictures) have.
fn trim(img: RgbImage) -> RgbImage {
    let (w, h) = img.dimensions();
    let dark_row = |y: u32| (0..w).all(|x| luma(img.get_pixel(x, y)) < 24);
    let dark_col = |x: u32| (0..h).all(|y| luma(img.get_pixel(x, y)) < 24);
    let (mut top, mut bottom) = (0, h);
    while top < h / 3 && dark_row(top) {
        top += 1;
    }
    while bottom > h - h / 3 && dark_row(bottom - 1) {
        bottom -= 1;
    }
    let (mut left, mut right) = (0, w);
    while left < w / 3 && dark_col(left) {
        left += 1;
    }
    while right > w - w / 3 && dark_col(right - 1) {
        right -= 1;
    }
    imageops::crop_imm(&img, left, top, right - left, bottom - top).to_image()
}

fn luma(p: &Rgb<u8>) -> u32 {
    (p[0] as u32 * 3 + p[1] as u32 * 6 + p[2] as u32) / 10
}

/// Centre square, at `SIZE`.
fn square(img: RgbImage) -> RgbImage {
    let (w, h) = img.dimensions();
    let side = w.min(h);
    let sq = imageops::crop_imm(&img, (w - side) / 2, (h - side) / 2, side, side).to_image();
    imageops::resize(&sq, SIZE, SIZE, FilterType::Triangle)
}

/// Where a square cover goes in `area`: as tall as it fits, centred.
/// A cell is two pixels tall, so `n` rows hold an `2n`×`2n` image in `2n` columns.
pub fn rect(area: Rect) -> Rect {
    let rows = area.height.min(area.width / 2);
    let cols = rows * 2;
    Rect { x: area.x + (area.width - cols) / 2, y: area.y + (area.height - rows) / 2, width: cols, height: rows }
}

fn color(p: &Rgb<u8>) -> Color {
    let d = |c: u8| (c as f32 * DIM) as u8;
    Color::Rgb(d(p[0]), d(p[1]), d(p[2]))
}

/// Draw `img` into `area` (see `rect`), dimmed.
pub fn draw(img: &RgbImage, area: Rect, buf: &mut Buffer) {
    if area.is_empty() {
        return;
    }
    let scaled = imageops::resize(img, area.width as u32, area.height as u32 * 2, FilterType::Triangle);
    for row in 0..area.height {
        for col in 0..area.width {
            let top = scaled.get_pixel(col as u32, row as u32 * 2);
            let bottom = scaled.get_pixel(col as u32, row as u32 * 2 + 1);
            buf[(area.x + col, area.y + row)].set_char('▀').set_fg(color(top)).set_bg(color(bottom));
        }
    }
}

/// Average of a cover cell's two pixels: the background for a cell the
/// visualizer draws over, since it replaces the `▀`.
pub fn blend(top: Color, bottom: Color) -> Color {
    match (top, bottom) {
        (Color::Rgb(r1, g1, b1), Color::Rgb(r2, g2, b2)) => {
            let m = |a: u8, b: u8| ((a as u16 + b as u16) / 2) as u8;
            Color::Rgb(m(r1, r2), m(g1, g2), m(b1, b2))
        }
        _ => bottom,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trims_letterbox() {
        // 4:3 frame, picture in the middle 16:9 band.
        let img = RgbImage::from_fn(48, 36, |_, y| if (5..31).contains(&y) { Rgb([200, 80, 40]) } else { Rgb([0, 0, 0]) });
        assert_eq!(trim(img).dimensions(), (48, 26));
    }

    #[test]
    fn cover_rect() {
        assert_eq!(rect(Rect::new(0, 0, 50, 10)), Rect::new(15, 0, 20, 10));
        assert_eq!(rect(Rect::new(0, 0, 8, 10)), Rect::new(0, 3, 8, 4));
    }
}
