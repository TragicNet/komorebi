use std::collections::HashMap;
use std::ffi::OsStr;
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::sync::OnceLock;

use crossbeam_channel::Receiver;
use crossbeam_channel::Sender;
use crossbeam_channel::unbounded;

use crate::DATA_DIR;
use crate::KomorebiTheme;
use crate::Wallpaper;
use crate::theme_manager;
use crate::windows_api::WindowsApi;
use komorebi_themes::Base16ColourPalette;
use komorebi_themes::KomorebiThemeCustom as Custom;
use komorebi_themes::ThemeVariant;

/// A snapshot of a wallpaper application request, taken entirely on the
/// window-manager thread so the worker never needs the WM lock.
///
/// The effective wallpaper (workspace override falling back to the monitor
/// wallpaper) is owned here, so the worker can run the slow Win32 shell
/// render and any uncached palette generation without touching shared WM state.
#[derive(Clone, Debug)]
pub struct WallpaperRequest {
    pub hmonitor: isize,
    pub wallpaper: Wallpaper,
}

/// Path of the on-disk base16 palette cache for a wallpaper/variant pair, e.g.
/// `<DATA_DIR>/catppuccin.base16.dark.json`.
pub fn palette_cache_path(path: &Path, variant: ThemeVariant) -> PathBuf {
    DATA_DIR.join(format!(
        "{}.base16.{variant}.json",
        path.file_name()
            .unwrap_or(OsStr::new("tmp"))
            .to_string_lossy()
    ))
}

/// Build the `Custom` theme for a wallpaper from a resolved palette, applying
/// the per-colour overrides from the wallpaper's theme options.
pub fn build_theme(wallpaper: &Wallpaper, palette: Base16ColourPalette) -> KomorebiTheme {
    KomorebiTheme::Custom(Custom {
        colours: Box::new(palette),
        single_border: wallpaper
            .theme_options
            .as_ref()
            .and_then(|o| o.single_border),
        stack_border: wallpaper
            .theme_options
            .as_ref()
            .and_then(|o| o.stack_border),
        monocle_border: wallpaper
            .theme_options
            .as_ref()
            .and_then(|o| o.monocle_border),
        floating_border: wallpaper
            .theme_options
            .as_ref()
            .and_then(|o| o.floating_border),
        pinned_border: wallpaper
            .theme_options
            .as_ref()
            .and_then(|o| o.pinned_border),
        unfocused_border: wallpaper
            .theme_options
            .as_ref()
            .and_then(|o| o.unfocused_border),
        unfocused_locked_border: wallpaper
            .theme_options
            .as_ref()
            .and_then(|o| o.unfocused_locked_border),
        stackbar_focused_text: wallpaper
            .theme_options
            .as_ref()
            .and_then(|o| o.stackbar_focused_text),
        stackbar_unfocused_text: wallpaper
            .theme_options
            .as_ref()
            .and_then(|o| o.stackbar_unfocused_text),
        stackbar_background: wallpaper
            .theme_options
            .as_ref()
            .and_then(|o| o.stackbar_background),
        bar_accent: wallpaper.theme_options.as_ref().and_then(|o| o.bar_accent),
    })
}

/// Applies wallpaper changes on a dedicated thread, never on the window-manager
/// thread.
///
/// `IDesktopWallpaper::SetWallpaper` asks the shell to decode and render the
/// image and repaint the desktop, which can block for a long time; generating a
/// base16 palette for an uncached wallpaper decodes the full image and is very
/// slow on debug builds. Both used to run inline in [`crate::workspace::Workspace`]'s
/// restore/retile path while the WM lock was held, stalling the whole switch.
///
/// The worker instead applies requests in submission order, coalescing a burst
/// of switches for the same monitor down to the last one (latest-wins), and
/// skips re-rendering a wallpaper that is already applied to that monitor. The
/// window-manager thread only clones the effective wallpaper and enqueues it.
pub struct WallpaperWorker;

impl WallpaperWorker {
    fn sender() -> Sender<WallpaperRequest> {
        static SENDER: OnceLock<Sender<WallpaperRequest>> = OnceLock::new();
        SENDER
            .get_or_init(|| {
                let (sender, receiver) = unbounded();
                std::thread::Builder::new()
                    .name("wallpaper-worker".into())
                    .spawn(move || Self::receive(receiver))
                    .expect("could not spawn wallpaper worker thread");
                sender
            })
            .clone()
    }

    /// Enqueue a wallpaper application request. This never blocks and never
    /// touches COM or the WM lock; the work happens on the worker thread.
    pub fn enqueue(request: WallpaperRequest) {
        if Self::sender().send(request).is_err() {
            tracing::warn!("could not enqueue wallpaper request: worker is gone");
        }
    }

    fn receive(receiver: Receiver<WallpaperRequest>) {
        // IDesktopWallpaper is an STA COM class; initialise the worker's own
        // apartment once so the cached thread-local interface stays in-process.
        WindowsApi::co_initialize_sta();

        tracing::info!("wallpaper worker listening");

        // The wallpaper currently applied to each monitor (canonical path),
        // seeded lazily from the OS so an already-displayed wallpaper is not
        // re-rendered on the first switch into a workspace.
        let mut applied: HashMap<isize, PathBuf> = HashMap::new();
        // The most recently themed effective wallpaper; re-theming a wallpaper
        // that already themed the borders/bar is a no-op.
        let mut last_themed: Option<Wallpaper> = None;

        while let Ok(request) = receiver.recv() {
            // Latest-wins coalescing: fold every queued request into a per-monitor
            // map so a burst of workspace switches renders only the final state.
            let mut batch: HashMap<isize, WallpaperRequest> = HashMap::new();
            batch.insert(request.hmonitor, request);
            while let Ok(request) = receiver.try_recv() {
                batch.insert(request.hmonitor, request);
            }

            for (hmonitor, request) in batch {
                Self::apply(hmonitor, request, &mut applied, &mut last_themed);
            }
        }
    }

    fn apply(
        hmonitor: isize,
        request: WallpaperRequest,
        applied: &mut HashMap<isize, PathBuf>,
        last_themed: &mut Option<Wallpaper>,
    ) {
        let WallpaperRequest { wallpaper, .. } = request;

        // Learn what the OS already displays the first time this monitor is
        // seen, then only re-render when the effective wallpaper differs.
        if !applied.contains_key(&hmonitor)
            && let Ok(current) = WindowsApi::get_wallpaper(hmonitor)
            && let Ok(current) = PathBuf::from(current).canonicalize()
        {
            applied.insert(hmonitor, current);
        }

        let wallpaper_changed = applied
            .get(&hmonitor)
            .is_none_or(|applied| applied != &wallpaper.path);

        if wallpaper_changed {
            if let Err(error) = WindowsApi::set_wallpaper(&wallpaper.path, hmonitor) {
                tracing::error!("failed to set wallpaper: {error}");
            } else if let Ok(canonical) = wallpaper.path.canonicalize() {
                applied.insert(hmonitor, canonical);
            }
        }

        // Theme the borders/the bar when the wallpaper changed or when this
        // wallpaper has never been themed this session (a cold start where the
        // OS already shows the image still needs the komorebi colours applied).
        if wallpaper.generate_theme.unwrap_or(true)
            && (wallpaper_changed || last_themed.as_ref() != Some(&wallpaper))
            && let Some(palette) = Self::resolve_palette(&wallpaper.path, &wallpaper)
        {
            let theme = build_theme(&wallpaper, palette);
            theme_manager::send_notification(theme);
            last_themed.replace(wallpaper);
        }
    }

    /// Resolve the base16 palette for a wallpaper, using the on-disk cache when
    /// present and generating (then atomically caching) it otherwise.
    fn resolve_palette(path: &Path, wallpaper: &Wallpaper) -> Option<Base16ColourPalette> {
        let variant = wallpaper
            .theme_options
            .as_ref()
            .and_then(|t| t.theme_variant)
            .unwrap_or_default();

        let cached_palette = palette_cache_path(path, variant);

        if cached_palette.is_file() {
            tracing::info!(
                "colour palette for wallpaper {} found in cache",
                cached_palette.display()
            );

            if let Ok(palette) = serde_json::from_str::<Base16ColourPalette>(
                &fs::read_to_string(&cached_palette).ok()?,
            ) {
                return Some(palette);
            }
        }

        tracing::info!(
            "colour palette for wallpaper {} was not cached, generating",
            path.display()
        );

        let palette = komorebi_themes::generate_base16_palette(path, variant).ok()?;

        match serde_json::to_string_pretty(&palette) {
            Ok(contents) => match Self::write_atomic(&cached_palette, &contents) {
                Ok(()) => tracing::info!(
                    "colour palette for wallpaper {} cached",
                    cached_palette.display()
                ),
                Err(error) => tracing::error!("failed to cache colour palette: {error}"),
            },
            Err(error) => tracing::error!("failed to serialise colour palette: {error}"),
        }

        Some(palette)
    }

    /// Write to a temporary sibling then rename, so a crash mid-write can never
    /// leave a partially-written palette that is indistinguishable from a valid
    /// cache entry.
    fn write_atomic(path: &Path, contents: &str) -> std::io::Result<()> {
        let tmp_path = path.with_extension("tmp");
        fs::write(&tmp_path, contents)?;
        if let Err(error) = fs::rename(&tmp_path, path) {
            let _ = fs::remove_file(&tmp_path);
            return Err(error);
        }
        Ok(())
    }
}
