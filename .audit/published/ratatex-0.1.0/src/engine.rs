use std::{
    collections::{HashMap, VecDeque},
    fmt, io,
    path::PathBuf,
    sync::{
        Arc, Mutex, PoisonError,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use crossbeam_channel::{Receiver, RecvTimeoutError, Sender, TrySendError, bounded};
use thiserror::Error;
use tracing::debug;

use crate::{
    Antialiasing, ColorScheme, GraphicsSupport, Limits, PixelPadding, TerminalProfile,
    config::{EngineConfig, Wake, default_cache_dir},
    kitty::{max_grid_dimension, placeholder_cells, upload_and_place},
    pipeline::{self, RenderedPng},
};

/// Why a formula could not be rendered.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RenderFailureKind {
    /// The in-process `RaTeX` parser, layout engine, or rasterizer rejected the formula.
    Ratex,
    /// TeX source exceeded a bound or used a denied primitive.
    UnsafeSource,
    /// A PNG was malformed or exceeded a configured bound.
    InvalidPng,
    /// The bounded background queue was temporarily full.
    Busy,
    /// The renderer has shut down.
    Shutdown,
}

/// Cloneable, typed rendering failure retained by the memory cache.
#[derive(Clone, Debug, Error)]
#[error("{message}")]
pub struct RenderFailure {
    kind: RenderFailureKind,
    message: Arc<str>,
}

impl RenderFailure {
    pub(crate) fn new(kind: RenderFailureKind, message: impl Into<Arc<str>>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    /// Stable failure category.
    pub const fn kind(&self) -> RenderFailureKind {
        self.kind
    }

    /// Human-readable diagnostic.
    pub fn message(&self) -> &str {
        &self.message
    }
}

/// Runtime capability summary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Availability {
    /// Kitty Unicode placeholders are available.
    pub graphics: bool,
}

impl Availability {
    /// Whether uncached formulas can be rendered and displayed.
    pub const fn fully_available(self) -> bool {
        self.graphics
    }
}

/// One immutable prepared formula.
pub struct Formula {
    image_id: u32,
    columns: u16,
    rows: u16,
    pixel_width: u32,
    pixel_height: u32,
    png: Arc<[u8]>,
    placeholders: Vec<Box<str>>,
    command: TerminalCommand,
    estimated_bytes: usize,
}

impl Formula {
    fn new(image_id: u32, rendered: RenderedPng, terminal: TerminalProfile) -> Arc<Self> {
        let png: Arc<[u8]> = rendered.png.into();
        let command = TerminalCommand(upload_and_place(
            &png,
            image_id,
            rendered.columns,
            rendered.rows,
            terminal.tmux,
        ));
        let placeholders = placeholder_cells(rendered.columns, rendered.rows, image_id);
        let estimated_bytes = png.len().saturating_add(command.len()).saturating_add(
            placeholders
                .iter()
                .map(|placeholder| placeholder.len())
                .sum::<usize>(),
        );
        Arc::new(Self {
            image_id,
            columns: rendered.columns,
            rows: rendered.rows,
            pixel_width: rendered.pixel_width,
            pixel_height: rendered.pixel_height,
            png,
            placeholders,
            command,
            estimated_bytes,
        })
    }

    /// Width reserved in terminal cells.
    pub const fn columns(&self) -> u16 {
        self.columns
    }

    /// Height reserved in terminal cells.
    pub const fn rows(&self) -> u16 {
        self.rows
    }

    /// Final cell-aligned PNG width.
    pub const fn pixel_width(&self) -> u32 {
        self.pixel_width
    }

    /// Final cell-aligned PNG height.
    pub const fn pixel_height(&self) -> u32 {
        self.pixel_height
    }

    /// Cached, post-processed PNG bytes.
    pub fn png(&self) -> &[u8] {
        &self.png
    }

    pub(crate) const fn image_id(&self) -> u32 {
        self.image_id
    }

    pub(crate) fn placeholder(&self, row: u16, column: u16) -> &str {
        let index = usize::from(row)
            .saturating_mul(usize::from(self.columns))
            .saturating_add(usize::from(column));
        self.placeholders
            .get(index)
            .map_or(" ", |placeholder| placeholder.as_ref())
    }

    #[cfg(test)]
    pub(crate) fn for_widget_test(image_id: u32, columns: u16, rows: u16) -> Arc<Self> {
        Self::new(
            image_id,
            RenderedPng {
                png: Vec::new(),
                columns,
                rows,
                pixel_width: u32::from(columns),
                pixel_height: u32::from(rows),
            },
            TerminalProfile::kitty(crate::PixelSize::new(1, 1), false),
        )
    }
}

impl fmt::Debug for Formula {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Formula")
            .field("image_id", &self.image_id)
            .field("columns", &self.columns)
            .field("rows", &self.rows)
            .field("pixel_width", &self.pixel_width)
            .field("pixel_height", &self.pixel_height)
            .finish_non_exhaustive()
    }
}

/// Current state of a formula request.
#[derive(Clone, Debug)]
pub enum FormulaState {
    /// The worker owns the request. Callers can keep a small blank/raw fallback.
    Pending,
    /// TeX and terminal data are ready.
    Ready(Arc<Formula>),
    /// A deterministic or cached render failure.
    Failed(Arc<RenderFailure>),
    /// The selected terminal does not support Unicode-placeholder graphics.
    Unsupported,
}

/// Bytes that must be written to the terminal outside the Ratatui buffer.
#[derive(Clone)]
pub struct TerminalCommand(Arc<[u8]>);

impl TerminalCommand {
    /// Raw Kitty/tmux command bytes.
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    /// Encoded byte count.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether the command is empty.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl fmt::Debug for TerminalCommand {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TerminalCommand")
            .field("bytes", &self.len())
            .finish()
    }
}

/// Builder for a background renderer.
#[must_use]
pub struct RatatexBuilder {
    terminal: TerminalProfile,
    colors: ColorScheme,
    antialiasing: Antialiasing,
    padding: PixelPadding,
    dpi: u16,
    cache_dir: PathBuf,
    limits: Limits,
    wake: Wake,
}

impl RatatexBuilder {
    /// Creates a renderer for a queried or manually supplied terminal profile.
    pub fn new(terminal: TerminalProfile) -> Self {
        Self {
            terminal,
            colors: ColorScheme::default(),
            antialiasing: Antialiasing::Grayscale,
            padding: PixelPadding::default(),
            dpi: 180,
            cache_dir: default_cache_dir(),
            limits: Limits::default(),
            wake: Arc::new(|| {}),
        }
    }

    /// Sets formula colors.
    pub const fn colors(mut self, colors: ColorScheme) -> Self {
        self.colors = colors;
        self
    }

    /// Sets glyph antialiasing.
    pub const fn antialiasing(mut self, antialiasing: Antialiasing) -> Self {
        self.antialiasing = antialiasing;
        self
    }

    /// Sets tight-bounds padding.
    pub const fn padding(mut self, padding: PixelPadding) -> Self {
        self.padding = padding;
        self
    }

    /// Sets the target DPI before 3× raster supersampling.
    pub const fn dpi(mut self, dpi: u16) -> Self {
        self.dpi = dpi;
        self
    }

    /// Sets the content-addressed PNG cache directory.
    pub fn cache_dir(mut self, cache_dir: impl Into<PathBuf>) -> Self {
        self.cache_dir = cache_dir.into();
        self
    }

    /// Replaces all resource bounds.
    pub fn limits(mut self, limits: Limits) -> Self {
        self.limits = limits;
        self
    }

    /// Installs a cheap callback used to wake the application's event loop.
    pub fn on_update(mut self, wake: impl Fn() + Send + Sync + 'static) -> Self {
        self.wake = Arc::new(wake);
        self
    }

    /// Starts bounded background workers.
    ///
    /// # Errors
    ///
    /// Returns an I/O error if an operating-system worker thread cannot be
    /// created.
    pub fn build(self) -> io::Result<Ratatex> {
        let limits = normalized_limits(self.limits);
        let availability = Availability {
            graphics: self.terminal.graphics == GraphicsSupport::Kitty,
        };
        let config = Arc::new(EngineConfig {
            terminal: self.terminal,
            colors: self.colors,
            antialiasing: self.antialiasing,
            padding: self.padding,
            dpi: self.dpi.clamp(72, 600),
            cache_dir: self.cache_dir,
            limits,
            wake: self.wake,
        });
        let (sender, receiver) = bounded(config.limits.request_queue);
        let shared = Arc::new(Shared::new(availability));
        let shutdown = Arc::new(AtomicBool::new(false));
        let mut workers = Vec::with_capacity(config.limits.workers);
        for index in 0..config.limits.workers {
            let worker_receiver = receiver.clone();
            let worker_shared = Arc::clone(&shared);
            let worker_config = Arc::clone(&config);
            let worker_shutdown = Arc::clone(&shutdown);
            workers.push(
                thread::Builder::new()
                    .name(format!("ratatex-{index}"))
                    .spawn(move || {
                        render_worker(
                            &worker_receiver,
                            &worker_shared,
                            &worker_config,
                            &worker_shutdown,
                        );
                    })?,
            );
        }
        Ok(Ratatex {
            owner: Arc::new(Owner {
                sender,
                shared,
                config,
                shutdown,
                workers: Mutex::new(workers),
            }),
        })
    }
}

/// Cloneable request/cache handle.
#[derive(Clone)]
pub struct Ratatex {
    owner: Arc<Owner>,
}

impl Ratatex {
    /// Starts a builder.
    pub fn builder(terminal: TerminalProfile) -> RatatexBuilder {
        RatatexBuilder::new(terminal)
    }

    /// Runtime capability summary.
    pub fn availability(&self) -> Availability {
        self.owner.shared.availability
    }

    /// Monotonic cache generation for width/layout memoization.
    pub fn generation(&self) -> u64 {
        self.owner.shared.generation.load(Ordering::Acquire)
    }

    /// Requests an expression at no more than `max_columns` terminal cells.
    ///
    /// This method never blocks on rendering and is intended to be called from
    /// a layout or render path.
    pub fn request(&self, source: &str, max_columns: u16) -> FormulaState {
        if self.owner.config.terminal.graphics != GraphicsSupport::Kitty {
            return FormulaState::Unsupported;
        }
        if self.owner.shutdown.load(Ordering::Acquire) {
            return FormulaState::Failed(Arc::new(RenderFailure::new(
                RenderFailureKind::Shutdown,
                "ratatex has shut down",
            )));
        }
        if let Err(failure) = pipeline::validate_source(source, &self.owner.config.limits) {
            return FormulaState::Failed(Arc::new(failure));
        }
        let max_columns = max_columns
            .max(1)
            .min(self.owner.config.limits.max_columns)
            .min(max_grid_dimension());
        let key = pipeline::cache_key(&self.owner.config, source, max_columns);
        {
            let mut cache = self
                .owner
                .shared
                .cache
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            cache.clock = cache.clock.saturating_add(1);
            let used_at = cache.clock;
            if let Some(entry) = cache.entries.get_mut(&key) {
                entry.touch(used_at);
                return entry.state();
            }
            cache.entries.insert(key, CacheEntry::Pending { used_at });
        }
        let request = RenderRequest {
            key,
            source: source.to_owned(),
            max_columns,
        };
        match self.owner.sender.try_send(request) {
            Ok(()) => FormulaState::Pending,
            Err(TrySendError::Full(_)) => {
                self.owner.shared.remove_pending(&key);
                FormulaState::Failed(Arc::new(RenderFailure::new(
                    RenderFailureKind::Busy,
                    "ratatex render queue is full; retry on the next frame",
                )))
            }
            Err(TrySendError::Disconnected(_)) => {
                self.owner.shared.remove_pending(&key);
                FormulaState::Failed(Arc::new(RenderFailure::new(
                    RenderFailureKind::Shutdown,
                    "ratatex render workers are unavailable",
                )))
            }
        }
    }

    /// Drains newly prepared image uploads/virtual placements.
    ///
    /// Write these bytes to the same terminal used by Ratatui before drawing
    /// the frame containing the corresponding [`FormulaWidget`](crate::FormulaWidget).
    pub fn drain_terminal_commands(&self) -> Vec<TerminalCommand> {
        self.owner
            .shared
            .commands
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .drain()
    }

    /// Requeues uploads after the terminal's alternate screen or graphics
    /// state has been recreated.
    ///
    /// Call this when a tmux pane regains focus as well. tmux normally drops
    /// passthrough sequences produced while the pane is not visible, even
    /// though it retains the corresponding Unicode placeholder cells.
    pub fn reupload_all(&self) {
        let commands = {
            let cache = self
                .owner
                .shared
                .cache
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            cache
                .entries
                .values()
                .filter_map(|entry| match entry {
                    CacheEntry::Ready { formula, .. } => Some(formula.command.clone()),
                    CacheEntry::Pending { .. } | CacheEntry::Failed { .. } => None,
                })
                .collect::<Vec<_>>()
        };
        if commands.is_empty() {
            return;
        }
        let mut pending = self
            .owner
            .shared
            .commands
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        for command in commands {
            pending.push(command, &self.owner.config.limits);
        }
        drop(pending);
        (self.owner.config.wake)();
    }

    /// Stops workers and waits for any bounded in-flight process.
    pub fn shutdown(&self) {
        self.owner.shutdown.store(true, Ordering::Release);
        join_workers(&self.owner.workers);
    }
}

struct Owner {
    sender: Sender<RenderRequest>,
    shared: Arc<Shared>,
    config: Arc<EngineConfig>,
    shutdown: Arc<AtomicBool>,
    workers: Mutex<Vec<JoinHandle<()>>>,
}

impl Drop for Owner {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        join_workers(&self.workers);
    }
}

fn join_workers(workers: &Mutex<Vec<JoinHandle<()>>>) {
    let workers = {
        let mut workers = workers.lock().unwrap_or_else(PoisonError::into_inner);
        std::mem::take(&mut *workers)
    };
    for worker in workers {
        let _ = worker.join();
    }
}

struct Shared {
    availability: Availability,
    cache: Mutex<MemoryCache>,
    commands: Mutex<CommandQueue>,
    generation: AtomicU64,
}

impl Shared {
    fn new(availability: Availability) -> Self {
        Self {
            availability,
            cache: Mutex::new(MemoryCache::default()),
            commands: Mutex::new(CommandQueue::default()),
            generation: AtomicU64::new(1),
        }
    }

    fn remove_pending(&self, key: &CacheKey) {
        let mut cache = self.cache.lock().unwrap_or_else(PoisonError::into_inner);
        if matches!(cache.entries.get(key), Some(CacheEntry::Pending { .. })) {
            cache.entries.remove(key);
        }
    }

    fn complete(
        &self,
        key: CacheKey,
        result: Result<RenderedPng, RenderFailure>,
        config: &EngineConfig,
    ) {
        let mut command = None;
        let entry = match result {
            Ok(rendered) => {
                let image_id = image_id_for_key(key);
                let formula = Formula::new(image_id, rendered, config.terminal);
                command = Some(formula.command.clone());
                CacheEntry::Ready {
                    estimated_bytes: formula.estimated_bytes,
                    formula,
                    used_at: 0,
                }
            }
            Err(failure) => {
                debug!(error = %failure, "ratatex formula render failed");
                CacheEntry::Failed {
                    failure: Arc::new(failure),
                    used_at: 0,
                }
            }
        };
        {
            let mut cache = self.cache.lock().unwrap_or_else(PoisonError::into_inner);
            cache.clock = cache.clock.saturating_add(1);
            let used_at = cache.clock;
            cache.insert(key, entry.with_used_at(used_at));
            cache.trim(&config.limits);
        }
        if let Some(command) = command {
            self.commands
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(command, &config.limits);
        }
        self.generation.fetch_add(1, Ordering::AcqRel);
        (config.wake)();
    }
}

fn image_id_for_key(key: CacheKey) -> u32 {
    let image_id = u32::from_be_bytes([key.0[0], key.0[1], key.0[2], key.0[3]]);
    image_id.max(1)
}

#[derive(Default)]
struct CommandQueue {
    commands: VecDeque<TerminalCommand>,
    total_bytes: usize,
}

impl CommandQueue {
    fn push(&mut self, command: TerminalCommand, limits: &Limits) {
        self.total_bytes = self.total_bytes.saturating_add(command.len());
        self.commands.push_back(command);
        while self.commands.len() > limits.max_memory_entries
            || self.total_bytes > limits.max_memory_bytes
        {
            let Some(removed) = self.commands.pop_front() else {
                break;
            };
            self.total_bytes = self.total_bytes.saturating_sub(removed.len());
        }
    }

    fn drain(&mut self) -> Vec<TerminalCommand> {
        self.total_bytes = 0;
        self.commands.drain(..).collect()
    }
}

#[derive(Default)]
struct MemoryCache {
    entries: HashMap<CacheKey, CacheEntry>,
    clock: u64,
    total_bytes: usize,
}

impl MemoryCache {
    fn insert(&mut self, key: CacheKey, entry: CacheEntry) {
        if let Some(previous) = self.entries.insert(key, entry) {
            self.total_bytes = self.total_bytes.saturating_sub(previous.estimated_bytes());
        }
        self.total_bytes = self.total_bytes.saturating_add(
            self.entries
                .get(&key)
                .map_or(0, CacheEntry::estimated_bytes),
        );
    }

    fn trim(&mut self, limits: &Limits) {
        while self.entries.len() > limits.max_memory_entries
            || self.total_bytes > limits.max_memory_bytes
        {
            let candidate = self
                .entries
                .iter()
                .filter(|(_, entry)| !matches!(entry, CacheEntry::Pending { .. }))
                .min_by_key(|(_, entry)| entry.used_at())
                .map(|(key, _)| *key);
            let Some(candidate) = candidate else {
                break;
            };
            if let Some(removed) = self.entries.remove(&candidate) {
                self.total_bytes = self.total_bytes.saturating_sub(removed.estimated_bytes());
            }
        }
    }
}

enum CacheEntry {
    Pending {
        used_at: u64,
    },
    Ready {
        formula: Arc<Formula>,
        used_at: u64,
        estimated_bytes: usize,
    },
    Failed {
        failure: Arc<RenderFailure>,
        used_at: u64,
    },
}

impl CacheEntry {
    const fn used_at(&self) -> u64 {
        match self {
            Self::Pending { used_at }
            | Self::Ready { used_at, .. }
            | Self::Failed { used_at, .. } => *used_at,
        }
    }

    const fn estimated_bytes(&self) -> usize {
        match self {
            Self::Ready {
                estimated_bytes, ..
            } => *estimated_bytes,
            Self::Pending { .. } | Self::Failed { .. } => 0,
        }
    }

    fn touch(&mut self, value: u64) {
        match self {
            Self::Pending { used_at }
            | Self::Ready { used_at, .. }
            | Self::Failed { used_at, .. } => *used_at = value,
        }
    }

    fn with_used_at(mut self, value: u64) -> Self {
        self.touch(value);
        self
    }

    fn state(&self) -> FormulaState {
        match self {
            Self::Pending { .. } => FormulaState::Pending,
            Self::Ready { formula, .. } => FormulaState::Ready(Arc::clone(formula)),
            Self::Failed { failure, .. } => FormulaState::Failed(Arc::clone(failure)),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct CacheKey(pub(crate) [u8; 32]);

struct RenderRequest {
    key: CacheKey,
    source: String,
    max_columns: u16,
}

fn render_worker(
    receiver: &Receiver<RenderRequest>,
    shared: &Arc<Shared>,
    config: &Arc<EngineConfig>,
    shutdown: &Arc<AtomicBool>,
) {
    while !shutdown.load(Ordering::Acquire) {
        match receiver.recv_timeout(Duration::from_millis(100)) {
            Ok(request) => {
                let result =
                    pipeline::render(config, request.key, &request.source, request.max_columns);
                shared.complete(request.key, result, config);
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
}

fn normalized_limits(mut limits: Limits) -> Limits {
    limits.max_source_bytes = limits.max_source_bytes.max(1);
    limits.max_png_bytes = limits.max_png_bytes.max(1024);
    limits.max_image_pixels = limits.max_image_pixels.max(1);
    limits.max_columns = limits.max_columns.clamp(1, max_grid_dimension());
    limits.max_rows = limits.max_rows.clamp(1, max_grid_dimension());
    limits.max_memory_entries = limits.max_memory_entries.max(1);
    limits.max_memory_bytes = limits.max_memory_bytes.max(1024);
    limits.max_disk_entries = limits.max_disk_entries.max(1);
    limits.request_queue = limits.request_queue.max(1);
    limits.workers = limits.workers.clamp(1, 8);
    limits
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::{CommandQueue, FormulaState, Ratatex, RenderFailureKind, TerminalCommand};
    use crate::{Limits, PixelSize, TerminalProfile};

    #[test]
    fn terminal_command_queue_is_bounded_by_entries_and_bytes() {
        let limits = Limits {
            max_memory_entries: 2,
            max_memory_bytes: 10,
            ..Limits::default()
        };
        let mut commands = CommandQueue::default();
        for byte in 0..3 {
            commands.push(TerminalCommand(vec![byte; 6].into()), &limits);
        }

        assert_eq!(commands.commands.len(), 1);
        assert_eq!(commands.total_bytes, 6);
        assert_eq!(commands.drain().len(), 1);
        assert_eq!(commands.total_bytes, 0);
    }

    #[test]
    fn unsupported_terminals_never_queue_work() {
        let renderer = Ratatex::builder(TerminalProfile::unsupported(PixelSize::default()))
            .build()
            .unwrap();
        assert!(matches!(
            renderer.request("x", 20),
            FormulaState::Unsupported
        ));
        renderer.shutdown();
    }

    #[test]
    fn unsafe_source_fails_before_the_worker() {
        let renderer = Ratatex::builder(TerminalProfile::kitty(PixelSize::default(), false))
            .build()
            .unwrap();
        let FormulaState::Failed(failure) = renderer.request(r"\input{/etc/passwd}", 20) else {
            panic!("unsafe source must fail");
        };
        assert_eq!(failure.kind(), RenderFailureKind::UnsafeSource);
        renderer.shutdown();
    }

    #[test]
    fn in_process_renderer_completes_background_work() {
        let (wake_tx, wake_rx) = crossbeam_channel::bounded(1);
        let cache = tempfile::tempdir().unwrap();
        let renderer = Ratatex::builder(TerminalProfile::kitty(PixelSize::default(), false))
            .cache_dir(cache.path())
            .on_update(move || {
                let _ = wake_tx.try_send(());
            })
            .build()
            .unwrap();
        assert!(matches!(renderer.request("x", 20), FormulaState::Pending));
        wake_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(matches!(renderer.request("x", 20), FormulaState::Ready(_)));
        assert!(renderer.availability().fully_available());
        renderer.shutdown();
    }

    #[test]
    fn common_display_math_renders_cold_in_process() {
        let formulas = [
            r"\rho\left(\frac{\partial \mathbf{u}}{\partial t}
                +(\mathbf{u}\cdot\nabla)\mathbf{u}\right)
                =-\nabla p+\mu\nabla^2\mathbf{u}+\rho\mathbf{f}",
            r"\int_0^\infty e^{-x^2}\,dx=\frac{\sqrt{\pi}}{2}",
            r"\sum_{k=0}^{n}k^2=\frac{n(n+1)(2n+1)}{6}",
            r"\begin{aligned}a&=b+c\\d&=e-f\end{aligned}",
            r"A=\begin{pmatrix}a&b\\c&d\end{pmatrix}",
        ];
        let (wake_tx, wake_rx) = crossbeam_channel::bounded(formulas.len());
        let cache = tempfile::tempdir().unwrap();
        let renderer = Ratatex::builder(TerminalProfile::kitty(PixelSize::default(), false))
            .cache_dir(cache.path())
            .on_update(move || {
                let _ = wake_tx.try_send(());
            })
            .build()
            .unwrap();

        let started = Instant::now();
        for source in formulas {
            assert!(matches!(
                renderer.request(source, 120),
                FormulaState::Pending
            ));
        }
        for _ in formulas {
            wake_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        }
        for source in formulas {
            let state = renderer.request(source, 120);
            assert!(
                matches!(state, FormulaState::Ready(_)),
                "in-process renderer did not support {source:?}: {state:?}"
            );
        }
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "cold in-process rendering regressed to subprocess-scale latency"
        );
        renderer.shutdown();
    }

    #[test]
    fn multiline_stress_formulas_reserve_compact_terminal_rows() {
        let formulas = [
            r"\begin{aligned}
                \nabla\cdot\mathbf{E} &= \frac{\rho}{\varepsilon_0}, &
                \nabla\cdot\mathbf{B} &= 0,\\
                \nabla\times\mathbf{E} &= -\frac{\partial\mathbf{B}}{\partial t}, &
                \nabla\times\mathbf{B} &= \mu_0\mathbf{J}
                +\mu_0\varepsilon_0\frac{\partial\mathbf{E}}{\partial t}.
                \end{aligned}",
            r"i\hbar\frac{\partial}{\partial t}\lvert\psi(t)\rangle
                =\hat{H}\lvert\psi(t)\rangle,\qquad
                \hat{H}=-\frac{\hbar^2}{2m}\nabla^2+V(\mathbf{x},t)",
            r"\begin{aligned}
                \frac{d}{dt}\left(\frac{\partial\mathcal{L}}{\partial\dot q_i}\right)
                -\frac{\partial\mathcal{L}}{\partial q_i}&=Q_i,\\[4pt]
                \mathcal{L}(q,\dot q,t)&=\frac{1}{2}\sum_{i,j=1}^{n}
                M_{ij}(q)\dot q_i\dot q_j-V(q,t),\\[4pt]
                \frac{\partial\mathcal{L}}{\partial\dot q_i}
                &=\sum_{j=1}^{n}M_{ij}(q)\dot q_j.
                \end{aligned}",
        ];
        let (wake_tx, wake_rx) = crossbeam_channel::bounded(formulas.len());
        let cache = tempfile::tempdir().unwrap();
        let renderer = Ratatex::builder(TerminalProfile::kitty(PixelSize::new(10, 20), false))
            .cache_dir(cache.path())
            .on_update(move || {
                let _ = wake_tx.try_send(());
            })
            .build()
            .unwrap();

        for source in formulas {
            assert!(matches!(
                renderer.request(source, 120),
                FormulaState::Pending
            ));
        }
        for _ in formulas {
            wake_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        }
        let rows = formulas.map(|source| {
            let FormulaState::Ready(formula) = renderer.request(source, 120) else {
                panic!("stress formula did not render");
            };
            formula.rows()
        });

        assert!(rows[0] <= 8, "Maxwell formula used {} rows", rows[0]);
        assert!(rows[1] <= 5, "Schrödinger formula used {} rows", rows[1]);
        assert!(rows[2] <= 16, "long aligned formula used {} rows", rows[2]);
        renderer.shutdown();
    }

    #[test]
    fn retained_nanocodex_formulas_fit_compact_row_budgets() {
        let formulas = [
            r"\left\{
                \begin{aligned}
                &i\hbar\frac{\partial}{\partial t}\Psi(\mathbf x,t)
                =\left[
                -\frac{\hbar^2}{2m}
                \left(\frac{1}{r^2}\frac{\partial}{\partial r}
                r^2\frac{\partial}{\partial r}
                -\frac{\widehat L^2}{\hbar^2r^2}\right)
                +V(r,t)
                \right]\Psi(\mathbf x,t),\\[4pt]
                &\Psi(\mathbf x,t)
                =\sum_{\ell=0}^{\infty}\sum_{m=-\ell}^{\ell}
                \frac{u_{\ell m}(r,t)}{r}\,
                Y_\ell^m(\theta,\varphi),\qquad
                \int_{\mathbb R^3}|\Psi|^2\,d^3x=1.
                \end{aligned}
                \right.",
            r"\begin{aligned}
                R^\rho{}_{\sigma\mu\nu}
                &=\partial_\mu\Gamma^\rho_{\nu\sigma}
                -\partial_\nu\Gamma^\rho_{\mu\sigma}
                +\Gamma^\rho_{\mu\lambda}\Gamma^\lambda_{\nu\sigma}
                -\Gamma^\rho_{\nu\lambda}\Gamma^\lambda_{\mu\sigma},\\
                \Gamma^\rho_{\mu\nu}
                &=\frac12g^{\rho\kappa}
                \left(\partial_\mu g_{\kappa\nu}
                +\partial_\nu g_{\kappa\mu}
                -\partial_\kappa g_{\mu\nu}\right),\\
                R_{\mu\nu}-\frac12R\,g_{\mu\nu}+\Lambda g_{\mu\nu}
                &=\frac{8\pi G}{c^4}T_{\mu\nu},
                \qquad
                \nabla_\mu T^{\mu\nu}=0.
                \end{aligned}",
        ];
        let (wake_tx, wake_rx) = crossbeam_channel::bounded(formulas.len());
        let cache = tempfile::tempdir().unwrap();
        let renderer = Ratatex::builder(TerminalProfile::kitty(PixelSize::new(14, 27), false))
            .cache_dir(cache.path())
            .on_update(move || {
                let _ = wake_tx.try_send(());
            })
            .build()
            .unwrap();

        for source in formulas {
            assert!(matches!(
                renderer.request(source, 54),
                FormulaState::Pending
            ));
        }
        for _ in formulas {
            wake_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        }
        let rows = formulas.map(|source| {
            let FormulaState::Ready(formula) = renderer.request(source, 54) else {
                panic!("retained formula did not render");
            };
            formula.rows()
        });

        assert!(rows[0] <= 7, "Schrödinger system used {} rows", rows[0]);
        assert!(rows[1] <= 7, "curvature system used {} rows", rows[1]);
        renderer.shutdown();
    }

    #[test]
    fn in_process_pipeline_renders_navier_stokes_and_reuses_disk_cache() {
        let source = r"\rho\left(\frac{\partial \mathbf{u}}{\partial t}
            +(\mathbf{u}\cdot\nabla)\mathbf{u}\right)
            =-\nabla p+\mu\nabla^2\mathbf{u}+\rho\mathbf{f}";
        let cache = tempfile::tempdir().unwrap();
        let (wake_tx, wake_rx) = crossbeam_channel::bounded(2);
        let renderer = Ratatex::builder(TerminalProfile::kitty(PixelSize::new(10, 20), false))
            .cache_dir(cache.path())
            .on_update(move || {
                let _ = wake_tx.try_send(());
            })
            .build()
            .unwrap();
        assert!(renderer.availability().fully_available());
        let cold_started = Instant::now();
        assert!(matches!(
            renderer.request(source, 120),
            FormulaState::Pending
        ));
        wake_rx.recv_timeout(Duration::from_secs(15)).unwrap();
        let cold_elapsed = cold_started.elapsed();
        let FormulaState::Ready(formula) = renderer.request(source, 120) else {
            panic!("live TeX render did not become ready");
        };
        assert!(formula.columns() > 10);
        assert!(formula.rows() > 1);
        assert_eq!(formula.pixel_width() % 10, 0);
        assert_eq!(formula.pixel_height() % 20, 0);
        assert!(!formula.png().is_empty());
        if let Some(path) = std::env::var_os("RATATEX_TEST_PNG") {
            std::fs::write(path, formula.png()).unwrap();
        }
        assert_eq!(renderer.drain_terminal_commands().len(), 1);
        renderer.shutdown();

        let (cached_tx, cached_rx) = crossbeam_channel::bounded(1);
        let cached = Ratatex::builder(TerminalProfile::kitty(PixelSize::new(10, 20), false))
            .cache_dir(cache.path())
            .on_update(move || {
                let _ = cached_tx.try_send(());
            })
            .build()
            .unwrap();
        let cached_started = Instant::now();
        assert!(matches!(cached.request(source, 120), FormulaState::Pending));
        cached_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        let cached_elapsed = cached_started.elapsed();
        assert!(matches!(
            cached.request(source, 120),
            FormulaState::Ready(_)
        ));
        eprintln!("cold={cold_elapsed:?} cached={cached_elapsed:?}");
        cached.shutdown();
    }
}
