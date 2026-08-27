use std::{env, path::PathBuf, sync::Arc};

use directories::BaseDirs;

/// An RGB color used by the raster post-processor.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct Rgb {
    /// Red channel.
    pub red: u8,
    /// Green channel.
    pub green: u8,
    /// Blue channel.
    pub blue: u8,
}

impl Rgb {
    /// Creates an RGB color.
    pub const fn new(red: u8, green: u8, blue: u8) -> Self {
        Self { red, green, blue }
    }

    pub(crate) const fn channels(self) -> [u8; 3] {
        [self.red, self.green, self.blue]
    }
}

/// Pixel dimensions of one terminal cell.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct PixelSize {
    /// Cell width in pixels.
    pub width: u16,
    /// Cell height in pixels.
    pub height: u16,
}

impl PixelSize {
    /// Creates a non-zero cell size.
    pub const fn new(width: u16, height: u16) -> Self {
        Self {
            width: if width == 0 { 1 } else { width },
            height: if height == 0 { 1 } else { height },
        }
    }
}

impl Default for PixelSize {
    fn default() -> Self {
        Self::new(10, 20)
    }
}

/// Terminal support for the image-placement half of the pipeline.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum GraphicsSupport {
    /// Kitty graphics with Unicode-placeholder placements is available.
    Kitty,
    /// The terminal did not report the required protocol.
    Unsupported,
}

/// Everything about the terminal that affects a rendered formula.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct TerminalProfile {
    /// Graphics protocol support.
    pub graphics: GraphicsSupport,
    /// Pixel size of one terminal cell.
    pub cell: PixelSize,
    /// Whether graphics commands need tmux passthrough escaping.
    pub tmux: bool,
}

impl TerminalProfile {
    /// Builds a manual Kitty profile.
    pub const fn kitty(cell: PixelSize, tmux: bool) -> Self {
        Self {
            graphics: GraphicsSupport::Kitty,
            cell,
            tmux,
        }
    }

    /// Builds a profile that always keeps the caller's text fallback.
    pub const fn unsupported(cell: PixelSize) -> Self {
        Self {
            graphics: GraphicsSupport::Unsupported,
            cell,
            tmux: false,
        }
    }

    /// Queries graphics support and cell size from the controlling terminal.
    ///
    /// Call this after entering the alternate screen and before starting a
    /// competing terminal-input reader.
    #[cfg(feature = "terminal-probe")]
    pub fn query(timeout: std::time::Duration) -> Self {
        use ratatui_image::picker::{Picker, ProtocolType, cap_parser::QueryStdioOptions};

        let options = QueryStdioOptions {
            timeout,
            ..QueryStdioOptions::default()
        };
        let tmux = env::var_os("TMUX").is_some();
        match Picker::from_query_stdio_with_options(options) {
            Ok(picker) => {
                let (cell_width, cell_height) = picker.font_size();
                Self {
                    graphics: if picker.protocol_type() == ProtocolType::Kitty {
                        GraphicsSupport::Kitty
                    } else {
                        GraphicsSupport::Unsupported
                    },
                    cell: PixelSize::new(cell_width, cell_height),
                    tmux,
                }
            }
            Err(_) => Self {
                graphics: GraphicsSupport::Unsupported,
                cell: PixelSize::default(),
                tmux,
            },
        }
    }
}

/// Formula antialiasing mode.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum Antialiasing {
    /// A transparent alpha mask, suitable for unknown or changing backgrounds.
    Grayscale,
    /// RGB LCD subpixel filtering composited onto the configured background.
    LcdRgb,
    /// BGR LCD subpixel filtering composited onto the configured background.
    LcdBgr,
}

/// Foreground/background colors used by raster post-processing.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct ColorScheme {
    /// Formula glyph color.
    pub foreground: Rgb,
    /// Background for LCD compositing. `None` forces transparent grayscale.
    pub background: Option<Rgb>,
}

impl Default for ColorScheme {
    fn default() -> Self {
        Self {
            foreground: Rgb::new(235, 235, 235),
            background: None,
        }
    }
}

/// Extra pixels around the tight TeX bounding box.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct PixelPadding {
    /// Pixels added on both left and right.
    pub horizontal: u16,
    /// Pixels added on both top and bottom.
    pub vertical: u16,
}

impl Default for PixelPadding {
    fn default() -> Self {
        Self {
            horizontal: 4,
            vertical: 2,
        }
    }
}

/// Resource and input bounds owned by the renderer.
#[derive(Clone, Debug)]
pub struct Limits {
    /// Maximum accepted TeX source bytes.
    pub max_source_bytes: usize,
    /// Maximum intermediate PNG bytes accepted from the renderer.
    pub max_png_bytes: usize,
    /// Maximum decoded pixels.
    pub max_image_pixels: u64,
    /// Maximum formula width in terminal cells.
    pub max_columns: u16,
    /// Maximum formula height in terminal cells.
    pub max_rows: u16,
    /// Maximum in-memory entries.
    pub max_memory_entries: usize,
    /// Maximum estimated in-memory cache bytes.
    pub max_memory_bytes: usize,
    /// Maximum disk-cache PNGs.
    pub max_disk_entries: usize,
    /// Maximum queued render requests.
    pub request_queue: usize,
    /// Number of independent render workers.
    pub workers: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_source_bytes: 16 * 1024,
            max_png_bytes: 8 * 1024 * 1024,
            max_image_pixels: 8 * 1024 * 1024,
            max_columns: 240,
            max_rows: 96,
            max_memory_entries: 128,
            max_memory_bytes: 64 * 1024 * 1024,
            max_disk_entries: 256,
            request_queue: 32,
            workers: 2,
        }
    }
}

pub(crate) type Wake = Arc<dyn Fn() + Send + Sync + 'static>;

#[derive(Clone)]
pub(crate) struct EngineConfig {
    pub terminal: TerminalProfile,
    pub colors: ColorScheme,
    pub antialiasing: Antialiasing,
    pub padding: PixelPadding,
    pub dpi: u16,
    pub cache_dir: PathBuf,
    pub limits: Limits,
    pub wake: Wake,
}

pub(crate) fn default_cache_dir() -> PathBuf {
    BaseDirs::new().map_or_else(
        || env::temp_dir().join("ratatex-cache"),
        |base| base.cache_dir().join("ratatex"),
    )
}
