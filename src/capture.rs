use std::{
    io::Cursor,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
        mpsc::{self, SyncSender},
    },
    thread,
};

use anyhow::{Context, Result, bail};
use fontdb::{Database, Family, Query, Stretch, Style as FontStyle, Weight};
use fontdue::{Font, FontSettings};
use ratatui::{
    buffer::Buffer,
    style::{Color, Modifier},
};
use unicode_width::UnicodeWidthStr;

use crate::control::{CaptureResult, CaptureView, ControlResponse};

const FONT_PX: f32 = 16.0;
const CELL_WIDTH: u32 = 10;
const CELL_HEIGHT: u32 = 20;
const MAX_SIDE: u32 = 4096;
const MAX_PIXELS: u64 = 16_000_000;
const MAX_PNG_BYTES: usize = 4 * 1024 * 1024;
const MAX_CAPTURE_JOBS: usize = 4;
const DEFAULT_FG: Rgb = Rgb(246, 241, 220);
const DEFAULT_BG: Rgb = Rgb(8, 17, 15);

const FALLBACK_REGULAR: &[u8] = include_bytes!("../assets/fonts/DejaVuSansMono.ttf");
const FALLBACK_BOLD: &[u8] = include_bytes!("../assets/fonts/DejaVuSansMono-Bold.ttf");
const FALLBACK_ITALIC: &[u8] = include_bytes!("../assets/fonts/DejaVuSansMono-Oblique.ttf");
const FALLBACK_BOLD_ITALIC: &[u8] =
    include_bytes!("../assets/fonts/DejaVuSansMono-BoldOblique.ttf");

pub struct CaptureJob {
    pub view: CaptureView,
    pub buffer: Buffer,
    pub cursor: Option<(u16, u16)>,
    pub reply: SyncSender<ControlResponse>,
}

pub struct CaptureWorker {
    tx: SyncSender<CaptureJob>,
    active: Arc<AtomicUsize>,
}

impl CaptureWorker {
    pub fn start() -> Self {
        let (tx, rx) = mpsc::sync_channel::<CaptureJob>(MAX_CAPTURE_JOBS);
        let active = Arc::new(AtomicUsize::new(0));
        let worker_active = Arc::clone(&active);
        thread::Builder::new()
            .name("demons-capture-encoder".to_owned())
            .spawn(move || {
                let fonts = FontSet::load();
                while let Ok(job) = rx.recv() {
                    let response = match fonts.as_ref() {
                        Ok(fonts) => render_png(&job.buffer, job.view, job.cursor, fonts)
                            .map(|capture| ControlResponse::Capture { capture })
                            .unwrap_or_else(|error| {
                                ControlResponse::error("capture_failed", format!("{error:#}"))
                            }),
                        Err(error) => ControlResponse::error("capture_failed", error.clone()),
                    };
                    job.reply.try_send(response).ok();
                    worker_active.fetch_sub(1, Ordering::AcqRel);
                }
            })
            .expect("failed to start TUI capture worker");
        Self { tx, active }
    }

    pub fn pending(&self) -> usize {
        self.active.load(Ordering::Acquire)
    }

    pub fn submit(&self, job: CaptureJob) -> Result<()> {
        if self
            .active
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |active| {
                (active < MAX_CAPTURE_JOBS).then_some(active + 1)
            })
            .is_err()
        {
            bail!("capture queue is full");
        }
        if self.tx.try_send(job).is_err() {
            self.active.fetch_sub(1, Ordering::AcqRel);
            bail!("capture queue is full");
        }
        Ok(())
    }
}

struct FontSet {
    primary: FontVariants,
    fallback: FontVariants,
    name: String,
}

struct FontVariants {
    regular: Font,
    bold: Font,
    italic: Font,
    bold_italic: Font,
}

impl FontSet {
    fn load() -> std::result::Result<Self, String> {
        let fallback = FontVariants::from_bundled().map_err(|error| format!("{error:#}"))?;
        match FontVariants::from_system() {
            Ok((primary, name)) => Ok(Self {
                primary,
                fallback,
                name: format!("{name} with DejaVu Sans Mono fallback"),
            }),
            Err(_) => Ok(Self {
                primary: FontVariants::from_bundled().map_err(|error| format!("{error:#}"))?,
                fallback,
                name: "DejaVu Sans Mono (bundled)".to_owned(),
            }),
        }
    }

    fn font(&self, modifier: Modifier, character: char) -> (&Font, bool) {
        let primary = self.primary.variant(modifier);
        if primary.lookup_glyph_index(character) != 0 || character == '\0' {
            return (primary, false);
        }
        let fallback = self.fallback.variant(modifier);
        (fallback, fallback.lookup_glyph_index(character) == 0)
    }
}

impl FontVariants {
    fn from_system() -> Result<(Self, String)> {
        let mut database = Database::new();
        database.load_system_fonts();
        let regular_id = database
            .query(&Query {
                families: &[Family::Monospace],
                weight: Weight::NORMAL,
                stretch: Stretch::Normal,
                style: FontStyle::Normal,
            })
            .context("no system monospace font found")?;
        let family = database
            .face(regular_id)
            .and_then(|face| face.families.first())
            .map(|family| family.0.clone())
            .context("system monospace font has no family name")?;
        let regular = font_from_database(&database, regular_id)?;
        let bold = query_family_font(&database, &family, Weight::BOLD, FontStyle::Normal)
            .unwrap_or_else(|| clone_font_from_database(&database, regular_id));
        let italic = query_family_font(&database, &family, Weight::NORMAL, FontStyle::Italic)
            .or_else(|| query_family_font(&database, &family, Weight::NORMAL, FontStyle::Oblique))
            .unwrap_or_else(|| clone_font_from_database(&database, regular_id));
        let bold_italic = query_family_font(&database, &family, Weight::BOLD, FontStyle::Italic)
            .or_else(|| query_family_font(&database, &family, Weight::BOLD, FontStyle::Oblique))
            .unwrap_or_else(|| clone_font_from_database(&database, regular_id));
        Ok((
            Self {
                regular,
                bold,
                italic,
                bold_italic,
            },
            family,
        ))
    }

    fn from_bundled() -> Result<Self> {
        Ok(Self {
            regular: font_from_bytes(FALLBACK_REGULAR, 0)?,
            bold: font_from_bytes(FALLBACK_BOLD, 0)?,
            italic: font_from_bytes(FALLBACK_ITALIC, 0)?,
            bold_italic: font_from_bytes(FALLBACK_BOLD_ITALIC, 0)?,
        })
    }

    fn variant(&self, modifier: Modifier) -> &Font {
        match (
            modifier.contains(Modifier::BOLD),
            modifier.contains(Modifier::ITALIC),
        ) {
            (true, true) => &self.bold_italic,
            (true, false) => &self.bold,
            (false, true) => &self.italic,
            (false, false) => &self.regular,
        }
    }
}

fn query_family_font(
    database: &Database,
    family: &str,
    weight: Weight,
    style: FontStyle,
) -> Option<Font> {
    let id = database.query(&Query {
        families: &[Family::Name(family)],
        weight,
        stretch: Stretch::Normal,
        style,
    })?;
    font_from_database(database, id).ok()
}

fn clone_font_from_database(database: &Database, id: fontdb::ID) -> Font {
    font_from_database(database, id).expect("font disappeared from system database")
}

fn font_from_database(database: &Database, id: fontdb::ID) -> Result<Font> {
    let (bytes, index) = database
        .with_face_data(id, |bytes, index| (bytes.to_vec(), index))
        .context("failed to load system font data")?;
    font_from_bytes(bytes, index)
}

fn font_from_bytes(bytes: impl AsRef<[u8]>, collection_index: u32) -> Result<Font> {
    Font::from_bytes(
        bytes.as_ref(),
        FontSettings {
            collection_index,
            ..FontSettings::default()
        },
    )
    .map_err(|error| anyhow::anyhow!(error))
}

fn render_png(
    buffer: &Buffer,
    view: CaptureView,
    cursor: Option<(u16, u16)>,
    fonts: &FontSet,
) -> Result<CaptureResult> {
    let columns = buffer.area.width;
    let rows = buffer.area.height;
    let width = u32::from(columns).saturating_mul(CELL_WIDTH);
    let height = u32::from(rows).saturating_mul(CELL_HEIGHT);
    if width == 0 || height == 0 {
        bail!("terminal frame is empty");
    }
    if width > MAX_SIDE || height > MAX_SIDE || u64::from(width) * u64::from(height) > MAX_PIXELS {
        bail!("terminal frame is too large to capture safely");
    }
    let mut pixels = vec![0_u8; width as usize * height as usize * 4];
    let mut missing_glyphs = 0;

    for row in 0..rows {
        let mut column = 0;
        while column < columns {
            let Some(cell) = buffer.cell((buffer.area.x + column, buffer.area.y + row)) else {
                column += 1;
                continue;
            };
            let mut fg = terminal_color(cell.fg, DEFAULT_FG);
            let mut bg = terminal_color(cell.bg, DEFAULT_BG);
            if cell.modifier.contains(Modifier::REVERSED) {
                std::mem::swap(&mut fg, &mut bg);
            }
            if cursor == Some((column, row)) {
                std::mem::swap(&mut fg, &mut bg);
            }
            if cell.modifier.contains(Modifier::DIM) {
                fg = blend(fg, bg, 0.58);
            }
            fill_cell(&mut pixels, width, column, row, 1, bg);
            let symbol = cell.symbol();
            let cell_span = UnicodeWidthStr::width(symbol).clamp(1, 2) as u16;
            if cell_span == 2 && column + 1 < columns {
                fill_cell(&mut pixels, width, column + 1, row, 1, bg);
            }
            if !cell.modifier.contains(Modifier::HIDDEN) && symbol != " " {
                draw_symbol(
                    &mut pixels,
                    width,
                    column,
                    row,
                    cell_span,
                    symbol,
                    fg,
                    cell.modifier,
                    fonts,
                    &mut missing_glyphs,
                );
            }
            if cell.modifier.contains(Modifier::UNDERLINED) {
                draw_rule(
                    &mut pixels,
                    width,
                    column,
                    row,
                    cell_span,
                    CELL_HEIGHT - 3,
                    fg,
                );
            }
            if cell.modifier.contains(Modifier::CROSSED_OUT) {
                draw_rule(
                    &mut pixels,
                    width,
                    column,
                    row,
                    cell_span,
                    CELL_HEIGHT / 2,
                    fg,
                );
            }
            column = column.saturating_add(cell_span.max(1));
        }
    }

    let mut png = Vec::new();
    {
        let mut encoder = png::Encoder::new(Cursor::new(&mut png), width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().context("failed to initialize PNG")?;
        writer
            .write_image_data(&pixels)
            .context("failed to encode PNG")?;
    }
    if png.len() > MAX_PNG_BYTES {
        bail!("encoded PNG exceeds the 4 MiB response limit");
    }
    Ok(CaptureResult {
        view,
        columns,
        rows,
        width,
        height,
        font: fonts.name.clone(),
        missing_glyphs,
        png,
    })
}

#[allow(clippy::too_many_arguments)]
fn draw_symbol(
    pixels: &mut [u8],
    image_width: u32,
    column: u16,
    row: u16,
    cell_span: u16,
    symbol: &str,
    color: Rgb,
    modifier: Modifier,
    fonts: &FontSet,
    missing_glyphs: &mut usize,
) {
    let baseline = (u32::from(row) * CELL_HEIGHT + 16) as i32;
    let origin_x = u32::from(column) * CELL_WIDTH;
    let target_width = u32::from(cell_span) * CELL_WIDTH;
    let mut advance = 0.0_f32;
    for character in symbol.chars() {
        if character.is_control() {
            continue;
        }
        let (font, missing) = fonts.font(modifier, character);
        if missing {
            *missing_glyphs += 1;
        }
        let (metrics, bitmap) = font.rasterize(character, FONT_PX);
        let centered = ((target_width as f32 - metrics.advance_width.max(1.0)) / 2.0).max(0.0);
        let x = origin_x as i32 + centered as i32 + advance as i32 + metrics.xmin;
        let y = baseline - metrics.ymin - metrics.height as i32;
        blend_bitmap(
            pixels,
            image_width,
            x,
            y,
            metrics.width,
            metrics.height,
            &bitmap,
            color,
        );
        if metrics.advance_width > 0.0 {
            advance += metrics.advance_width;
        }
    }
}

fn fill_cell(pixels: &mut [u8], image_width: u32, column: u16, row: u16, span: u16, color: Rgb) {
    let start_x = u32::from(column) * CELL_WIDTH;
    let end_x = start_x + u32::from(span) * CELL_WIDTH;
    let start_y = u32::from(row) * CELL_HEIGHT;
    for y in start_y..start_y + CELL_HEIGHT {
        for x in start_x..end_x {
            set_pixel(pixels, image_width, x, y, color, 255);
        }
    }
}

fn draw_rule(
    pixels: &mut [u8],
    image_width: u32,
    column: u16,
    row: u16,
    span: u16,
    offset_y: u32,
    color: Rgb,
) {
    let start_x = u32::from(column) * CELL_WIDTH;
    let end_x = start_x + u32::from(span) * CELL_WIDTH;
    let y = u32::from(row) * CELL_HEIGHT + offset_y;
    for x in start_x..end_x {
        set_pixel(pixels, image_width, x, y, color, 255);
    }
}

#[allow(clippy::too_many_arguments)]
fn blend_bitmap(
    pixels: &mut [u8],
    image_width: u32,
    x: i32,
    y: i32,
    bitmap_width: usize,
    bitmap_height: usize,
    bitmap: &[u8],
    color: Rgb,
) {
    let image_height = pixels.len() as u32 / 4 / image_width;
    for bitmap_y in 0..bitmap_height {
        for bitmap_x in 0..bitmap_width {
            let target_x = x + bitmap_x as i32;
            let target_y = y + bitmap_y as i32;
            if target_x < 0
                || target_y < 0
                || target_x >= image_width as i32
                || target_y >= image_height as i32
            {
                continue;
            }
            let alpha = bitmap[bitmap_y * bitmap_width + bitmap_x];
            set_pixel(
                pixels,
                image_width,
                target_x as u32,
                target_y as u32,
                color,
                alpha,
            );
        }
    }
}

fn set_pixel(pixels: &mut [u8], image_width: u32, x: u32, y: u32, color: Rgb, alpha: u8) {
    let index = ((y * image_width + x) * 4) as usize;
    if alpha == 255 {
        pixels[index..index + 4].copy_from_slice(&[color.0, color.1, color.2, 255]);
        return;
    }
    let inverse = 255_u16 - u16::from(alpha);
    pixels[index] =
        ((u16::from(color.0) * u16::from(alpha) + u16::from(pixels[index]) * inverse) / 255) as u8;
    pixels[index + 1] = ((u16::from(color.1) * u16::from(alpha)
        + u16::from(pixels[index + 1]) * inverse)
        / 255) as u8;
    pixels[index + 2] = ((u16::from(color.2) * u16::from(alpha)
        + u16::from(pixels[index + 2]) * inverse)
        / 255) as u8;
    pixels[index + 3] = 255;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Rgb(u8, u8, u8);

fn blend(foreground: Rgb, background: Rgb, amount: f32) -> Rgb {
    let component = |foreground: u8, background: u8| {
        (f32::from(foreground) * amount + f32::from(background) * (1.0 - amount)) as u8
    };
    Rgb(
        component(foreground.0, background.0),
        component(foreground.1, background.1),
        component(foreground.2, background.2),
    )
}

fn terminal_color(color: Color, reset: Rgb) -> Rgb {
    match color {
        Color::Reset => reset,
        Color::Black => Rgb(0, 0, 0),
        Color::Red => Rgb(205, 49, 49),
        Color::Green => Rgb(13, 188, 121),
        Color::Yellow => Rgb(229, 229, 16),
        Color::Blue => Rgb(36, 114, 200),
        Color::Magenta => Rgb(188, 63, 188),
        Color::Cyan => Rgb(17, 168, 205),
        Color::Gray => Rgb(229, 229, 229),
        Color::DarkGray => Rgb(102, 102, 102),
        Color::LightRed => Rgb(241, 76, 76),
        Color::LightGreen => Rgb(35, 209, 139),
        Color::LightYellow => Rgb(245, 245, 67),
        Color::LightBlue => Rgb(59, 142, 234),
        Color::LightMagenta => Rgb(214, 112, 214),
        Color::LightCyan => Rgb(41, 184, 219),
        Color::White => Rgb(255, 255, 255),
        Color::Rgb(red, green, blue) => Rgb(red, green, blue),
        Color::Indexed(index) => xterm_color(index),
    }
}

fn xterm_color(index: u8) -> Rgb {
    const BASE: [Rgb; 16] = [
        Rgb(0, 0, 0),
        Rgb(205, 0, 0),
        Rgb(0, 205, 0),
        Rgb(205, 205, 0),
        Rgb(0, 0, 238),
        Rgb(205, 0, 205),
        Rgb(0, 205, 205),
        Rgb(229, 229, 229),
        Rgb(127, 127, 127),
        Rgb(255, 0, 0),
        Rgb(0, 255, 0),
        Rgb(255, 255, 0),
        Rgb(92, 92, 255),
        Rgb(255, 0, 255),
        Rgb(0, 255, 255),
        Rgb(255, 255, 255),
    ];
    match index {
        0..=15 => BASE[index as usize],
        16..=231 => {
            let value = index - 16;
            let red = value / 36;
            let green = (value % 36) / 6;
            let blue = value % 6;
            let channel = |value: u8| if value == 0 { 0 } else { 55 + 40 * value };
            Rgb(channel(red), channel(green), channel(blue))
        }
        232..=255 => {
            let gray = 8 + 10 * (index - 232);
            Rgb(gray, gray, gray)
        }
    }
}

#[cfg(test)]
mod tests {
    use ratatui::{buffer::Buffer, layout::Rect, style::Color};

    use super::*;

    #[test]
    fn renders_a_small_buffer_as_png() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 3, 2));
        buffer[(0, 0)].set_symbol("A").set_fg(Color::Red);
        buffer[(1, 0)].set_symbol("界").set_fg(Color::Green);
        let fonts = FontSet::load().unwrap();
        let capture = render_png(&buffer, CaptureView::Full, Some((0, 0)), &fonts).unwrap();
        assert_eq!((capture.width, capture.height), (30, 40));
        assert!(capture.png.starts_with(b"\x89PNG\r\n\x1a\n"));
    }

    #[test]
    fn maps_xterm_color_cube_and_grayscale() {
        assert_eq!(xterm_color(16), Rgb(0, 0, 0));
        assert_eq!(xterm_color(231), Rgb(255, 255, 255));
        assert_eq!(xterm_color(232), Rgb(8, 8, 8));
        assert_eq!(xterm_color(255), Rgb(238, 238, 238));
    }
}
