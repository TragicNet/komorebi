use crate::AnimationStyle;
use crate::FLOATING_APPLICATIONS;
use crate::FLOATING_WINDOW_TOGGLE_ASPECT_RATIO;
use crate::HIDDEN_HWNDS;
use crate::HIDING_BEHAVIOUR;
use crate::IGNORE_IDENTIFIERS;
use crate::LAYERED_WHITELIST;
use crate::MANAGE_IDENTIFIERS;
use crate::NO_TITLEBAR;
use crate::PERMAIGNORE_CLASSES;
use crate::REGEX_IDENTIFIERS;
use crate::SLOW_APPLICATION_COMPENSATION_TIME;
use crate::SLOW_APPLICATION_IDENTIFIERS;
use crate::WSL2_UI_PROCESSES;
use crate::animation::ANIMATION_DURATION_GLOBAL;
use crate::animation::ANIMATION_DURATION_PER_ANIMATION;
use crate::animation::ANIMATION_ENABLED_GLOBAL;
use crate::animation::ANIMATION_ENABLED_PER_ANIMATION;
use crate::animation::ANIMATION_MANAGER;
use crate::animation::ANIMATION_STYLE_GLOBAL;
use crate::animation::ANIMATION_STYLE_PER_ANIMATION;
use crate::animation::AnimationEngine;
use crate::animation::GHOST_MOVEMENT_ENABLED;
use crate::animation::RenderDispatcher;
use crate::animation::ghost::GhostWindow;
use crate::animation::lerp::Lerp;
use crate::animation::prefix::AnimationPrefix;
use crate::animation::prefix::new_animation_key;
use crate::border_manager;
use crate::com::SetCloak;
use crate::core::ApplicationIdentifier;
use crate::core::HidingBehaviour;
use crate::core::Rect;
use crate::core::config_generation::IdWithIdentifier;
use crate::core::config_generation::MatchingRule;
use crate::core::config_generation::MatchingStrategy;
use crate::focus_manager;
use crate::stackbar_manager;
use crate::styles::ExtendedWindowStyle;
use crate::styles::WindowStyle;
use crate::transparency_manager;
use crate::window_manager_event::WindowManagerEvent;
use crate::windows_api;
use crate::windows_api::WindowsApi;
use color_eyre::eyre;
use crossbeam_utils::atomic::AtomicConsume;
use parking_lot::Mutex;
use regex::Regex;
use serde::Deserialize;
use serde::Serialize;
use serde::Serializer;
use serde::ser::SerializeStruct;
use std::collections::HashMap;
use std::convert::TryFrom;
use std::fmt::Display;
use std::fmt::Formatter;
use std::fmt::Write as _;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicI32;
use std::sync::atomic::Ordering;
use std::thread;
use std::time::Duration;
use strum::Display;
use strum::EnumString;
use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Gdi::HMONITOR;

pub static MINIMUM_WIDTH: AtomicI32 = AtomicI32::new(0);
pub static MINIMUM_HEIGHT: AtomicI32 = AtomicI32::new(0);

#[derive(Debug, Default, Clone, Copy, Deserialize, PartialEq)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct Window {
    pub hwnd: isize,
}

impl From<isize> for Window {
    fn from(value: isize) -> Self {
        Self { hwnd: value }
    }
}

impl From<HWND> for Window {
    fn from(value: HWND) -> Self {
        Self {
            hwnd: value.0 as isize,
        }
    }
}

#[allow(clippy::module_name_repetitions)]
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct WindowDetails {
    pub title: String,
    pub exe: String,
    pub class: String,
}

impl TryFrom<Window> for WindowDetails {
    type Error = eyre::ErrReport;

    fn try_from(value: Window) -> std::result::Result<Self, Self::Error> {
        Ok(Self {
            title: value.title()?,
            exe: value.exe()?,
            class: value.class()?,
        })
    }
}

impl Display for Window {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let mut display = format!("(hwnd: {}", self.hwnd);

        if let Ok(title) = self.title() {
            write!(display, ", title: {title}")?;
        }

        if let Ok(exe) = self.exe() {
            write!(display, ", exe: {exe}")?;
        }

        if let Ok(class) = self.class() {
            write!(display, ", class: {class}")?;
        }

        write!(display, ")")?;

        write!(f, "{display}")
    }
}

impl Serialize for Window {
    fn serialize<S>(&self, serializer: S) -> eyre::Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut state = serializer.serialize_struct("Window", 5)?;
        state.serialize_field("hwnd", &self.hwnd)?;
        state.serialize_field(
            "title",
            &self
                .title()
                .unwrap_or_else(|_| String::from("could not get window title")),
        )?;
        state.serialize_field(
            "exe",
            &self
                .exe()
                .unwrap_or_else(|_| String::from("could not get window exe")),
        )?;
        state.serialize_field(
            "class",
            &self
                .class()
                .unwrap_or_else(|_| String::from("could not get window class")),
        )?;
        state.serialize_field(
            "rect",
            &WindowsApi::window_rect(self.hwnd).unwrap_or_default(),
        )?;
        state.end()
    }
}

struct MovementRenderDispatcher {
    hwnd: isize,
    start_rect: Rect,
    target_rect: Rect,
    top: bool,
    style: AnimationStyle,
    /// Some between successful pre_render and post_render/cleanup_on_cancel when
    /// ghost movement is active. None for the legacy code path.
    ghost: Mutex<Option<GhostWindow>>,
    /// Tracks whether the source has been cloaked so cleanup can uncloak idempotently.
    cloaked: AtomicBool,
    /// Last lerped logical rect actually applied; used by cleanup_on_cancel to
    /// snap the real window to the position the user was last seeing.
    last_animated_rect: Mutex<Rect>,
    /// True when pre_render successfully repositioned the source to target_rect
    /// before registering the thumbnail. In that case post_render must skip
    /// the final position_window since the source is already there.
    pre_painted: AtomicBool,
}

impl MovementRenderDispatcher {
    const PREFIX: AnimationPrefix = AnimationPrefix::Movement;

    pub fn new(
        hwnd: isize,
        start_rect: Rect,
        target_rect: Rect,
        top: bool,
        style: AnimationStyle,
    ) -> Self {
        Self {
            hwnd,
            start_rect,
            target_rect,
            top,
            style,
            ghost: Mutex::new(None),
            cloaked: AtomicBool::new(false),
            last_animated_rect: Mutex::new(start_rect),
            pre_painted: AtomicBool::new(false),
        }
    }

    fn use_ghost(&self) -> bool {
        GHOST_MOVEMENT_ENABLED.load(Ordering::Relaxed)
    }

    fn size_changes_during_animation(&self) -> bool {
        !self.start_rect.is_same_size_as(&self.target_rect)
    }

    fn animated_rect(&self, progress: f64) -> Rect {
        if self.size_changes_during_animation() {
            Rect {
                left: self
                    .start_rect
                    .left
                    .lerp(self.target_rect.left, progress, self.style),
                top: self
                    .start_rect
                    .top
                    .lerp(self.target_rect.top, progress, self.style),
                right: self.start_rect.right,
                bottom: self.start_rect.bottom,
            }
        } else {
            self.start_rect.lerp(self.target_rect, progress, self.style)
        }
    }

    /// Chromium / Electron windows expose a top-level class beginning with
    /// `Chrome_WidgetWin_`. Their renderer pipeline is suspended whenever
    /// `NativeWindowOcclusionTrackerWin` reads any non-zero `DWMWA_CLOAKED`
    /// state on the HWND, so the pre-paint trick (cloak → SetWindowPos →
    /// capture) leaves the DComp swap chain stale and the post-uncloak frame
    /// shows half-painted / black regions. For these apps we fall back to
    /// capture-at-start: keep the source cloaked at start_rect for the whole
    /// animation and only move it to target in post_render, where the
    /// uncloak is the visibility flip that wakes Viz back up.
    fn source_is_chromium_shell(&self) -> bool {
        WindowsApi::real_window_class_w(self.hwnd)
            .map(|class| class.starts_with("Chrome_WidgetWin_"))
            .unwrap_or(false)
    }

    fn finalise_managers(&self) {
        if ANIMATION_MANAGER
            .lock()
            .count_in_progress(MovementRenderDispatcher::PREFIX)
            == 0
        {
            if WindowsApi::foreground_window().unwrap_or_default() == self.hwnd {
                focus_manager::send_notification(self.hwnd)
            }

            stackbar_manager::STACKBAR_TEMPORARILY_DISABLED.store(false, Ordering::SeqCst);

            stackbar_manager::send_notification();
            transparency_manager::send_notification();
        }
    }
}

impl RenderDispatcher for MovementRenderDispatcher {
    fn get_animation_key(&self) -> String {
        new_animation_key(MovementRenderDispatcher::PREFIX, self.hwnd.to_string())
    }

    fn pre_render(&self) -> eyre::Result<()> {
        stackbar_manager::STACKBAR_TEMPORARILY_DISABLED.store(true, Ordering::SeqCst);
        stackbar_manager::send_notification();

        if self.use_ghost() {
            let is_chromium = self.source_is_chromium_shell();
            let size_changes = self.size_changes_during_animation();

            // The ghost host is sized to the LOGICAL rect (visible content
            // area). DWM thumbnails capture the source at its
            // DWMWA_EXTENDED_FRAME_BOUNDS extents (visible content), not
            // GetWindowRect outer extents that include the drop-shadow
            // margin. Sizing the host to outer dims would stretch the
            // visible-content texture by the shadow ratio.
            //
            // Place the ghost in z-order immediately above the source so
            // multiple simultaneously animating windows (workspace switches,
            // layout flips) keep the same relative stacking as their
            // sources rather than all piling up at HWND_TOP in creation
            // order.
            //
            // For non-Chromium sources we ALSO pre-position the source to
            // target_rect *before* registering the thumbnail, so the
            // captured pixels reflect target-dimensioned content. The ghost
            // dest then animates start → target with the texture
            // downscaling to native 1:1 at the end — crisp final frame
            // instead of an upscaled blur. For Chromium we skip pre-paint
            // (see `source_is_chromium_shell`).
            //
            // DwmSetWindowAttribute(DWMWA_CLOAK) is rejected with
            // E_ACCESSDENIED for foreign HWNDs; the undocumented
            // IApplicationView::SetCloak path used elsewhere does not have
            // that restriction.
            SetCloak(Window { hwnd: self.hwnd }.hwnd(), 1, 2);
            self.cloaked.store(true, Ordering::SeqCst);

            if !is_chromium && !size_changes {
                if let Err(error) =
                    WindowsApi::position_window_async(self.hwnd, &self.target_rect, self.top, false)
                {
                    tracing::warn!(
                        "ghost movement: failed to pre-position hwnd {}: {error}",
                        self.hwnd
                    );
                } else {
                    // No DwmFlush here. DWM thumbnails are live: once
                    // registered, the thumbnail surface updates as the
                    // source paints, so the texture catches up to
                    // target-dim content within the first frame or two of
                    // the animation. Skipping the flush avoids a ~16ms
                    // pre-render stall on every non-Chromium animation.
                    self.pre_painted.store(true, Ordering::SeqCst);
                }
            }

            match GhostWindow::create(self.hwnd, self.start_rect, Some(self.hwnd)) {
                Ok(ghost) => {
                    *self.ghost.lock() = Some(ghost);
                }
                Err(error) => {
                    tracing::warn!(
                        "ghost movement: failed to create ghost for hwnd {}: {error}; \
                         uncloaking and falling back to legacy path",
                        self.hwnd
                    );
                    SetCloak(Window { hwnd: self.hwnd }.hwnd(), 1, 0);
                    self.cloaked.store(false, Ordering::SeqCst);
                }
            }
        }

        Ok(())
    }

    fn render(&self, progress: f64) -> eyre::Result<()> {
        let logical = self.animated_rect(progress);
        *self.last_animated_rect.lock() = logical;

        let ghost_active = self.ghost.lock().is_some();
        if ghost_active {
            if let Some(ghost) = self.ghost.lock().as_ref()
                && let Err(error) = ghost.update_rect(logical)
            {
                tracing::trace!("ghost update_rect failed: {error}");
            }
            border_manager::animate_to(self.hwnd, logical);
        } else {
            // Legacy path: animations always run on a separate thread. Move the
            // window with an always-async SetWindowPos so we never block on the
            // target window's WindowProc thread; a slow/hung app must not stall
            // the animation slot (which previously wedged the whole arbitration
            // chain forever).
            WindowsApi::position_window_async(self.hwnd, &logical, false, true)?;
            WindowsApi::invalidate_rect(self.hwnd, None, false);
        }

        Ok(())
    }

    fn post_render(&self, is_current: bool) -> eyre::Result<()> {
        // If we lost the slot before reaching post_render (force-released just
        // as the animation completed), a successor may already be operating on
        // this hwnd. Never reposition, uncloak, or fade then; only discard our
        // own ghost thumbnail.
        if !is_current {
            if let Some(ghost) = self.ghost.lock().take() {
                let _ = ghost.dispose();
            }
            return Ok(());
        }

        let used_ghost = self.ghost.lock().is_some();
        let pre_painted = self.pre_painted.load(Ordering::SeqCst);
        let size_changes = self.size_changes_during_animation();

        // Final single SetWindowPos. For the pre-paint ghost path the source
        // has already been moved to target_rect in pre_render and we skip
        // this. For the Chromium ghost path (no pre-paint) the source is
        // still cloaked at start_rect and needs to be moved here. For the
        // legacy non-ghost path this is the original final reposition.
        //
        // Async so even this final call can't block the animation thread
        // forever on an unresponsive app (see position_window_async).
        if !pre_painted {
            WindowsApi::position_window_async(self.hwnd, &self.target_rect, self.top, false)?;
        }

        // Uncloak BEFORE crossfade so the real window's first post-resize
        // frame is being composed underneath the still-visible ghost while
        // we fade. This gives Chromium/Electron renderers time to produce a
        // CompositorFrame at the new size — the visibility flip from
        // cloaked-to-uncloaked is what nudges Viz to resume frame
        // production.
        if self.cloaked.swap(false, Ordering::SeqCst) {
            SetCloak(Window { hwnd: self.hwnd }.hwnd(), 1, 0);
        }

        if used_ghost {
            if size_changes
                && let Some(ghost) = self.ghost.lock().as_ref()
                && let Err(error) = ghost.update_rect(self.target_rect)
            {
                tracing::trace!("ghost final update_rect failed: {error}");
            }

            // Crossfade the ghost out over several DWM frames. This masks the
            // texture mismatch (start-dim bitmap stretched vs. crisp
            // target-dim repaint) and gives slow-to-repaint apps time to
            // present their first post-resize frame before the overlay is
            // removed. Mirrors KWin's geometry-effect crossfade.
            //
            // Ease-in curve (1 - t^3): opacity holds high for most of the
            // fade and only drops sharply at the end. The ghost stays
            // prominent while the real window's first few frames land
            // underneath, so the user perceives a smooth reveal rather than
            // a snap.
            //
            // We call set_opacity directly (synchronous DwmUpdateThumbnailProperties
            // on this thread) rather than via the ghost owner channel, so
            // each step is guaranteed to be visible before the following
            // DwmFlush waits for the next vblank.
            if let Some(ghost) = self.ghost.lock().as_ref() {
                const FADE_STEPS: u32 = 8;
                for step in 1..=FADE_STEPS {
                    let t = step as f32 / FADE_STEPS as f32;
                    let progress = t * t * t;
                    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                    let opacity_u8 = ((1.0 - progress) * 255.0).round().clamp(0.0, 255.0) as u8;
                    let _ = ghost.set_opacity(opacity_u8);
                    unsafe {
                        let _ = windows::Win32::Graphics::Dwm::DwmFlush();
                    }
                }
            }
        } else {
            // Legacy path: still benefit from one DWM frame's wait so the
            // app's first post-move paint lands.
            unsafe {
                let _ = windows::Win32::Graphics::Dwm::DwmFlush();
            }
        }

        if let Some(ghost) = self.ghost.lock().take() {
            let _ = ghost.dispose();
        }

        self.finalise_managers();

        Ok(())
    }

    fn cleanup_on_cancel(&self) {
        // Snap the real window to wherever the ghost was last drawn so the next
        // dispatcher can capture an accurate start_rect. Then uncloak and tear
        // down the ghost. Mirrors post_render but uses last_animated_rect.
        let target = *self.last_animated_rect.lock();

        if let Err(error) = WindowsApi::position_window_async(self.hwnd, &target, false, false) {
            tracing::warn!(
                "ghost movement cancel: failed to snap hwnd {} to last rect: {error}",
                self.hwnd
            );
        }

        if self.cloaked.swap(false, Ordering::SeqCst) {
            SetCloak(Window { hwnd: self.hwnd }.hwnd(), 1, 0);
        }

        if let Some(ghost) = self.ghost.lock().take() {
            let _ = ghost.dispose();
        }

        self.finalise_managers();
    }

    /// The render slot was taken away (force-release) and a newer animation
    /// may already be running on this hwnd, so we must not reposition,
    /// uncloak, or fade. The successor owns that state from here on (it
    /// re-cloaks and creates its own ghost in its own pre_render). Only our
    /// own DWM thumbnail can be disposed safely.
    fn on_superseded(&self) {
        if let Some(ghost) = self.ghost.lock().take() {
            let _ = ghost.dispose();
        }
    }
}

struct TransparencyRenderDispatcher {
    hwnd: isize,
    start_opacity: u8,
    target_opacity: u8,
    style: AnimationStyle,
    is_opaque: bool,
}

impl TransparencyRenderDispatcher {
    const PREFIX: AnimationPrefix = AnimationPrefix::Transparency;

    pub fn new(
        hwnd: isize,
        is_opaque: bool,
        start_opacity: u8,
        target_opacity: u8,
        style: AnimationStyle,
    ) -> Self {
        Self {
            hwnd,
            start_opacity,
            target_opacity,
            style,
            is_opaque,
        }
    }
}

impl RenderDispatcher for TransparencyRenderDispatcher {
    fn get_animation_key(&self) -> String {
        new_animation_key(TransparencyRenderDispatcher::PREFIX, self.hwnd.to_string())
    }

    fn pre_render(&self) -> eyre::Result<()> {
        //transparent
        if !self.is_opaque {
            let window = Window::from(self.hwnd);
            let mut ex_style = window.ex_style()?;
            ex_style.insert(ExtendedWindowStyle::LAYERED);
            window.update_ex_style(&ex_style)?;
        }

        Ok(())
    }

    fn render(&self, progress: f64) -> eyre::Result<()> {
        WindowsApi::set_transparent(
            self.hwnd,
            self.start_opacity
                .lerp(self.target_opacity, progress, self.style),
        )
    }

    fn post_render(&self, is_current: bool) -> eyre::Result<()> {
        // The render slot was taken away; the successor owns the window state.
        if !is_current {
            return Ok(());
        }

        //opaque
        if self.is_opaque {
            let window = Window::from(self.hwnd);
            let mut ex_style = window.ex_style()?;
            ex_style.remove(ExtendedWindowStyle::LAYERED);
            window.update_ex_style(&ex_style)?;
        }

        Ok(())
    }
}

#[derive(Copy, Clone, Debug, Display, EnumString, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(untagged)]
/// Aspect ratio for temporarily floating windows
pub enum AspectRatio {
    /// Predefined aspect ratio
    #[cfg_attr(feature = "schemars", schemars(title = "Predefined"))]
    Predefined(PredefinedAspectRatio),
    /// Custom W:H aspect ratio
    #[cfg_attr(feature = "schemars", schemars(title = "Custom"))]
    Custom(i32, i32),
}

impl Default for AspectRatio {
    fn default() -> Self {
        AspectRatio::Predefined(PredefinedAspectRatio::default())
    }
}

#[derive(Copy, Clone, Debug, Default, Display, EnumString, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
/// Predefined aspect ratio
pub enum PredefinedAspectRatio {
    /// 21:9
    Ultrawide,
    /// 16:9
    Widescreen,
    /// 4:3
    #[default]
    Standard,
}

impl AspectRatio {
    pub fn width_and_height(self) -> (i32, i32) {
        match self {
            AspectRatio::Predefined(predefined) => match predefined {
                PredefinedAspectRatio::Ultrawide => (21, 9),
                PredefinedAspectRatio::Widescreen => (16, 9),
                PredefinedAspectRatio::Standard => (4, 3),
            },
            AspectRatio::Custom(w, h) => (w, h),
        }
    }
}

pub(crate) const FULLSCREEN_EDGE_TOLERANCE: i32 = 8;

/// Lenient edge tolerance used when checking coverage of a monitor's work area:
/// borderless games sized to the work area can land a few pixels short or over
/// the taskbar edge.
pub(crate) const FULLSCREEN_WORK_AREA_EDGE_TOLERANCE: i32 = 8;

/// Minimum share of the target area a window must cover to count as fullscreen
/// coverage, in case its edges are not aligned with the monitor (e.g. a window
/// spanning two monitors or snapped with a small gap). Low enough that a game
/// sized to the work area of a full-HD monitor with a 30px taskbar (≈97.9% of
/// the monitor area) still counts as fullscreen coverage.
pub(crate) const FULLSCREEN_COVERAGE_RATIO: f32 = 0.97;

/// Checks whether `window_rect` covers `monitor_rect` within `tolerance` pixels
/// on every edge. Pure helper so the fullscreen decision can be unit tested
/// without touching the Win32 API.
pub(crate) fn rect_covers_monitor_rect(
    window_rect: &Rect,
    monitor_rect: &Rect,
    tolerance: i32,
) -> bool {
    window_rect.left <= monitor_rect.left + tolerance
        && window_rect.top <= monitor_rect.top + tolerance
        && window_rect.right >= monitor_rect.right - tolerance
        && window_rect.bottom >= monitor_rect.bottom - tolerance
}

/// Checks whether `window_rect` covers `target_rect` either by covering it
/// within `tolerance` pixels on every edge, or by overlapping at least
/// `coverage_ratio` of its area. Both rectangles must be expressed in the same
/// absolute coordinate space. Pure helper so the fullscreen-coverage decision
/// can be unit tested without touching the Win32 API.
pub(crate) fn rect_covers_target_rect(
    window_rect: &Rect,
    target_rect: &Rect,
    tolerance: i32,
    coverage_ratio: f32,
) -> bool {
    if rect_covers_monitor_rect(window_rect, target_rect, tolerance) {
        return true;
    }

    let overlap_width =
        (window_rect.right.min(target_rect.right) - window_rect.left.max(target_rect.left)).max(0);
    let overlap_height =
        (window_rect.bottom.min(target_rect.bottom) - window_rect.top.max(target_rect.top)).max(0);
    let target_width = (target_rect.right - target_rect.left).max(1);
    let target_height = (target_rect.bottom - target_rect.top).max(1);

    let covered = u64::try_from(overlap_width).unwrap_or_default()
        * u64::try_from(overlap_height).unwrap_or_default();
    let total = u64::try_from(target_width).unwrap_or_default()
        * u64::try_from(target_height).unwrap_or_default();

    (covered as f32) / (total as f32) >= coverage_ratio
}

impl Window {
    const FLOATING_WINDOW_RESIZE_MARGIN: i32 = 10;

    pub const fn hwnd(self) -> HWND {
        HWND(windows_api::as_ptr!(self.hwnd))
    }

    pub fn floating_resize_safe_area(
        work_area: &Rect,
        window_width: i32,
        window_height: i32,
    ) -> Rect {
        let mut safe_area = *work_area;

        let horizontal_margin = if window_width < work_area.right {
            ((work_area.right - window_width) / 2).min(Self::FLOATING_WINDOW_RESIZE_MARGIN)
        } else {
            0
        };

        let vertical_margin = if window_height < work_area.bottom {
            ((work_area.bottom - window_height) / 2).min(Self::FLOATING_WINDOW_RESIZE_MARGIN)
        } else {
            0
        };

        safe_area.left += horizontal_margin;
        safe_area.top += vertical_margin;
        safe_area.right -= horizontal_margin * 2;
        safe_area.bottom -= vertical_margin * 2;

        safe_area
    }

    pub fn move_to_area(&mut self, current_area: &Rect, target_area: &Rect) -> eyre::Result<()> {
        let current_rect = WindowsApi::window_rect(self.hwnd)?;
        let current_area =
            Self::floating_resize_safe_area(current_area, current_rect.right, current_rect.bottom);
        let target_area =
            Self::floating_resize_safe_area(target_area, current_rect.right, current_rect.bottom);
        let x_diff = target_area.left - current_area.left;
        let y_diff = target_area.top - current_area.top;
        let x_ratio = f32::abs((target_area.right as f32) / (current_area.right as f32));
        let y_ratio = f32::abs((target_area.bottom as f32) / (current_area.bottom as f32));
        let window_relative_x = current_rect.left - current_area.left;
        let window_relative_y = current_rect.top - current_area.top;
        let corrected_relative_x = (window_relative_x as f32 * x_ratio) as i32;
        let corrected_relative_y = (window_relative_y as f32 * y_ratio) as i32;
        let window_x = current_area.left + corrected_relative_x;
        let window_y = current_area.top + corrected_relative_y;
        let left = x_diff + window_x;
        let top = y_diff + window_y;

        let corrected_width = (current_rect.right as f32 * x_ratio) as i32;
        let corrected_height = (current_rect.bottom as f32 * y_ratio) as i32;

        let new_rect = Rect {
            left,
            top,
            right: corrected_width,
            bottom: corrected_height,
        };

        let is_maximized = new_rect == target_area;
        if is_maximized {
            windows_api::WindowsApi::unmaximize_window(self.hwnd);
            let animation_enabled = ANIMATION_ENABLED_PER_ANIMATION.lock();
            let move_enabled = animation_enabled
                .get(&MovementRenderDispatcher::PREFIX)
                .is_some_and(|v| *v);
            drop(animation_enabled);

            if move_enabled || ANIMATION_ENABLED_GLOBAL.load(Ordering::SeqCst) {
                let anim_count = ANIMATION_MANAGER
                    .lock()
                    .count_in_progress(MovementRenderDispatcher::PREFIX);
                self.set_position(&new_rect, true)?;
                let hwnd = self.hwnd;
                // Wait for the animation to finish before maximizing the window again, otherwise
                // we would be maximizing the window on the current monitor anyway
                thread::spawn(move || {
                    let mut new_anim_count = ANIMATION_MANAGER
                        .lock()
                        .count_in_progress(MovementRenderDispatcher::PREFIX);
                    let mut max_wait = 2000; // Max waiting time. No one will be using an animation longer than 2s, right? RIGHT??? WHY?
                    while new_anim_count > anim_count && max_wait > 0 {
                        thread::sleep(Duration::from_millis(10));
                        new_anim_count = ANIMATION_MANAGER
                            .lock()
                            .count_in_progress(MovementRenderDispatcher::PREFIX);
                        max_wait -= 1;
                    }
                    windows_api::WindowsApi::maximize_window(hwnd);
                });
            } else {
                self.set_position(&new_rect, true)?;
                windows_api::WindowsApi::maximize_window(self.hwnd);
            }
        } else {
            self.set_position(&new_rect, true)?;
        }

        Ok(())
    }

    pub fn center(&mut self, work_area: &Rect, resize: bool) -> eyre::Result<()> {
        let (target_width, target_height) = if resize {
            let (aspect_ratio_width, aspect_ratio_height) = FLOATING_WINDOW_TOGGLE_ASPECT_RATIO
                .lock()
                .width_and_height();
            let target_height = work_area.bottom / 2;
            let target_width = (target_height * aspect_ratio_width) / aspect_ratio_height;
            (target_width, target_height)
        } else {
            let current_rect = WindowsApi::window_rect(self.hwnd)?;
            (current_rect.right, current_rect.bottom)
        };

        let safe_work_area =
            Self::floating_resize_safe_area(work_area, target_width, target_height);
        let x = safe_work_area.left + ((safe_work_area.right - target_width) / 2);
        let y = safe_work_area.top + ((safe_work_area.bottom - target_height) / 2);

        self.set_position(
            &Rect {
                left: x,
                top: y,
                right: target_width,
                bottom: target_height,
            },
            true,
        )
    }

    pub fn set_position(&self, layout: &Rect, top: bool) -> eyre::Result<()> {
        let window_rect = WindowsApi::window_rect(self.hwnd)?;

        if window_rect.eq(layout) {
            return Ok(());
        }

        let animation_enabled = ANIMATION_ENABLED_PER_ANIMATION.lock();
        let move_enabled = animation_enabled.get(&MovementRenderDispatcher::PREFIX);

        if move_enabled.is_some_and(|enabled| *enabled)
            || ANIMATION_ENABLED_GLOBAL.load(Ordering::SeqCst)
        {
            let duration = Duration::from_millis(
                *ANIMATION_DURATION_PER_ANIMATION
                    .lock()
                    .get(&MovementRenderDispatcher::PREFIX)
                    .unwrap_or(&ANIMATION_DURATION_GLOBAL.load(Ordering::SeqCst)),
            );
            let style = *ANIMATION_STYLE_PER_ANIMATION
                .lock()
                .get(&MovementRenderDispatcher::PREFIX)
                .unwrap_or(&ANIMATION_STYLE_GLOBAL.lock());

            let render_dispatcher =
                MovementRenderDispatcher::new(self.hwnd, window_rect, *layout, top, style);

            AnimationEngine::animate(render_dispatcher, duration)
        } else {
            WindowsApi::position_window(self.hwnd, layout, top, true, false)
        }
    }

    pub fn is_maximized(self) -> bool {
        WindowsApi::is_zoomed(self.hwnd)
    }

    pub fn is_minimized(self) -> bool {
        WindowsApi::is_iconic(self.hwnd)
    }

    pub fn is_miminized(self) -> bool {
        self.is_minimized()
    }

    pub fn is_visible(self) -> bool {
        WindowsApi::is_window_visible(self.hwnd)
    }

    /// Whether the window is actually rendered on screen: `is_window_visible`
    /// combined with not being minimized or DWM-cloaked (the `Cloak` and
    /// `Minimize` hiding behaviours leave `IsWindowVisible` true).
    pub fn is_shown(self) -> bool {
        WindowsApi::is_window_shown(self.hwnd)
    }

    pub fn hide_with_border(self, hide_border: bool) {
        let mut programmatically_hidden_hwnds = HIDDEN_HWNDS.lock();
        if !programmatically_hidden_hwnds.contains(&self.hwnd) {
            programmatically_hidden_hwnds.push(self.hwnd);
        }

        let hiding_behaviour = HIDING_BEHAVIOUR.lock();

        #[allow(deprecated)]
        match *hiding_behaviour {
            HidingBehaviour::Hide => WindowsApi::hide_window(self.hwnd),
            HidingBehaviour::Minimize => WindowsApi::minimize_window(self.hwnd),
            HidingBehaviour::Cloak => SetCloak(self.hwnd(), 1, 2),
        }
        if hide_border {
            border_manager::hide_border(self.hwnd);
        }
    }

    pub fn hide(self) {
        self.hide_with_border(true);
    }

    pub fn restore_with_border(self, restore_border: bool) {
        let mut programmatically_hidden_hwnds = HIDDEN_HWNDS.lock();
        if let Some(idx) = programmatically_hidden_hwnds
            .iter()
            .position(|&hwnd| hwnd == self.hwnd)
        {
            programmatically_hidden_hwnds.remove(idx);
        }

        let hiding_behaviour = HIDING_BEHAVIOUR.lock();

        #[allow(deprecated)]
        match *hiding_behaviour {
            HidingBehaviour::Hide | HidingBehaviour::Minimize => {
                // Use synchronous ShowWindow to ensure the window state (minimized, visible)
                // is updated immediately. Async ShowWindowAsync can leave the window in a
                // minimized state when subsequent code checks is_minimized() (IsIconic),
                // causing workspace.update() to incorrectly remove the window.
                WindowsApi::restore_window_sync(self.hwnd);
            }
            HidingBehaviour::Cloak => SetCloak(self.hwnd(), 1, 0),
        }
        if restore_border {
            border_manager::show_border(self.hwnd);
        }
    }

    pub fn restore(self) {
        self.restore_with_border(true);
    }

    pub fn minimize(self) {
        let exe = self.exe().unwrap_or_default();
        if !exe.contains("komorebi-bar") {
            WindowsApi::minimize_window(self.hwnd);
        }
    }

    pub fn close(self) -> eyre::Result<()> {
        WindowsApi::close_window(self.hwnd)
    }

    pub fn maximize(self) {
        let mut programmatically_hidden_hwnds = HIDDEN_HWNDS.lock();
        if let Some(idx) = programmatically_hidden_hwnds
            .iter()
            .position(|&hwnd| hwnd == self.hwnd)
        {
            programmatically_hidden_hwnds.remove(idx);
        }

        WindowsApi::maximize_window(self.hwnd);
    }

    pub fn unmaximize(self) {
        let mut programmatically_hidden_hwnds = HIDDEN_HWNDS.lock();
        if let Some(idx) = programmatically_hidden_hwnds
            .iter()
            .position(|&hwnd| hwnd == self.hwnd)
        {
            programmatically_hidden_hwnds.remove(idx);
        }

        WindowsApi::unmaximize_window(self.hwnd);
    }

    pub fn focus(self, mouse_follows_focus: bool) -> eyre::Result<()> {
        // If the target window is already focused, do nothing.
        if let Ok(ihwnd) = WindowsApi::foreground_window()
            && ihwnd == self.hwnd
        {
            // Center cursor in Window
            if mouse_follows_focus {
                WindowsApi::center_cursor_in_rect(&WindowsApi::window_rect(self.hwnd)?)?;
            }

            return Ok(());
        }

        WindowsApi::raise_and_focus_window(self.hwnd)?;

        // Center cursor in Window
        if mouse_follows_focus {
            WindowsApi::center_cursor_in_rect(&WindowsApi::window_rect(self.hwnd)?)?;
        }

        Ok(())
    }

    pub fn is_focused(self) -> bool {
        WindowsApi::foreground_window().unwrap_or_default() == self.hwnd
    }

    pub fn transparent(self) -> eyre::Result<()> {
        let animation_enabled = ANIMATION_ENABLED_PER_ANIMATION.lock();
        let transparent_enabled = animation_enabled.get(&TransparencyRenderDispatcher::PREFIX);

        if transparent_enabled.is_some_and(|enabled| *enabled)
            || ANIMATION_ENABLED_GLOBAL.load(Ordering::SeqCst)
        {
            let duration = Duration::from_millis(
                *ANIMATION_DURATION_PER_ANIMATION
                    .lock()
                    .get(&TransparencyRenderDispatcher::PREFIX)
                    .unwrap_or(&ANIMATION_DURATION_GLOBAL.load(Ordering::SeqCst)),
            );
            let style = *ANIMATION_STYLE_PER_ANIMATION
                .lock()
                .get(&TransparencyRenderDispatcher::PREFIX)
                .unwrap_or(&ANIMATION_STYLE_GLOBAL.lock());

            let render_dispatcher = TransparencyRenderDispatcher::new(
                self.hwnd,
                false,
                WindowsApi::get_transparent(self.hwnd).unwrap_or(255),
                transparency_manager::TRANSPARENCY_ALPHA.load_consume(),
                style,
            );

            AnimationEngine::animate(render_dispatcher, duration)
        } else {
            let target_alpha = transparency_manager::TRANSPARENCY_ALPHA.load_consume();
            let mut ex_style = self.ex_style()?;

            // Skip redundant calls when the window is already layered at the target alpha; the
            // marshalled SetWindowLongPtrW / SetLayeredWindowAttributes calls are expensive and can
            // block on the target window's thread.
            if ex_style.contains(ExtendedWindowStyle::LAYERED)
                && WindowsApi::get_transparent(self.hwnd)
                    .map(|alpha| alpha == target_alpha)
                    .unwrap_or(false)
            {
                return Ok(());
            }

            ex_style.insert(ExtendedWindowStyle::LAYERED);
            self.update_ex_style(&ex_style)?;
            WindowsApi::set_transparent(self.hwnd, target_alpha)
        }
    }

    pub fn opaque(self) -> eyre::Result<()> {
        let animation_enabled = ANIMATION_ENABLED_PER_ANIMATION.lock();
        let transparent_enabled = animation_enabled.get(&TransparencyRenderDispatcher::PREFIX);

        if transparent_enabled.is_some_and(|enabled| *enabled)
            || ANIMATION_ENABLED_GLOBAL.load(Ordering::SeqCst)
        {
            let duration = Duration::from_millis(
                *ANIMATION_DURATION_PER_ANIMATION
                    .lock()
                    .get(&TransparencyRenderDispatcher::PREFIX)
                    .unwrap_or(&ANIMATION_DURATION_GLOBAL.load(Ordering::SeqCst)),
            );
            let style = *ANIMATION_STYLE_PER_ANIMATION
                .lock()
                .get(&TransparencyRenderDispatcher::PREFIX)
                .unwrap_or(&ANIMATION_STYLE_GLOBAL.lock());

            let render_dispatcher = TransparencyRenderDispatcher::new(
                self.hwnd,
                true,
                WindowsApi::get_transparent(self.hwnd)
                    .unwrap_or(transparency_manager::TRANSPARENCY_ALPHA.load_consume()),
                255,
                style,
            );

            AnimationEngine::animate(render_dispatcher, duration)
        } else {
            let mut ex_style = self.ex_style()?;

            // Skip redundant SetWindowLongPtrW calls when the window is already opaque (not layered).
            if !ex_style.contains(ExtendedWindowStyle::LAYERED) {
                return Ok(());
            }

            ex_style.remove(ExtendedWindowStyle::LAYERED);
            self.update_ex_style(&ex_style)
        }
    }

    pub fn set_accent(self, colour: u32) -> eyre::Result<()> {
        WindowsApi::set_window_accent(self.hwnd, Some(colour))
    }

    pub fn remove_accent(self) -> eyre::Result<()> {
        WindowsApi::set_window_accent(self.hwnd, None)
    }

    #[cfg(target_pointer_width = "64")]
    pub fn update_style(self, style: &WindowStyle) -> eyre::Result<()> {
        WindowsApi::update_style(self.hwnd, isize::try_from(style.bits())?)
    }

    #[cfg(target_pointer_width = "32")]
    pub fn update_style(self, style: &WindowStyle) -> eyre::Result<()> {
        WindowsApi::update_style(self.hwnd, i32::try_from(style.bits())?)
    }

    #[cfg(target_pointer_width = "64")]
    pub fn update_ex_style(self, style: &ExtendedWindowStyle) -> eyre::Result<()> {
        WindowsApi::update_ex_style(self.hwnd, isize::try_from(style.bits())?)
    }

    #[cfg(target_pointer_width = "32")]
    pub fn update_ex_style(self, style: &ExtendedWindowStyle) -> eyre::Result<()> {
        WindowsApi::update_ex_style(self.hwnd, i32::try_from(style.bits())?)
    }

    pub fn style(self) -> eyre::Result<WindowStyle> {
        let bits = u32::try_from(WindowsApi::gwl_style(self.hwnd)?)?;
        Ok(WindowStyle::from_bits_truncate(bits))
    }

    pub fn ex_style(self) -> eyre::Result<ExtendedWindowStyle> {
        let bits = u32::try_from(WindowsApi::gwl_ex_style(self.hwnd)?)?;
        Ok(ExtendedWindowStyle::from_bits_truncate(bits))
    }

    pub fn title(self) -> eyre::Result<String> {
        WindowsApi::window_text_w(self.hwnd)
    }

    pub fn path(self) -> eyre::Result<String> {
        let (process_id, _) = WindowsApi::window_thread_process_id(self.hwnd);
        let handle = WindowsApi::process_handle(process_id)?;
        let path = WindowsApi::exe_path(handle);
        WindowsApi::close_process(handle)?;
        path
    }

    pub fn exe(self) -> eyre::Result<String> {
        let (process_id, _) = WindowsApi::window_thread_process_id(self.hwnd);
        let handle = WindowsApi::process_handle(process_id)?;
        let exe = WindowsApi::exe(handle);
        WindowsApi::close_process(handle)?;
        exe
    }

    pub fn process_id(self) -> u32 {
        let (process_id, _) = WindowsApi::window_thread_process_id(self.hwnd);
        process_id
    }

    pub fn class(self) -> eyre::Result<String> {
        WindowsApi::real_window_class_w(self.hwnd)
    }

    pub fn is_cloaked(self) -> eyre::Result<bool> {
        WindowsApi::is_window_cloaked(self.hwnd)
    }

    pub fn is_window(self) -> bool {
        WindowsApi::is_window(self.hwnd)
    }

    pub fn remove_title_bar(self) -> eyre::Result<()> {
        let mut style = self.style()?;
        style.remove(WindowStyle::CAPTION);
        style.remove(WindowStyle::THICKFRAME);
        self.update_style(&style)
    }

    pub fn add_title_bar(self) -> eyre::Result<()> {
        let mut style = self.style()?;
        style.insert(WindowStyle::CAPTION);
        style.insert(WindowStyle::THICKFRAME);
        self.update_style(&style)
    }

    /// Raise the window to the top of the Z order, but do not activate or focus
    /// it. Use raise_and_focus_window to activate and focus a window.
    /// It also checks if there is a border attached to this window and if it is
    /// it raises it as well.
    pub fn raise(self) -> eyre::Result<()> {
        WindowsApi::raise_window(self.hwnd)?;
        if let Some(border_info) = crate::border_manager::window_border(self.hwnd) {
            WindowsApi::raise_window(border_info.border_hwnd)?;
        }
        Ok(())
    }

    /// Raise the window to the top of the Z order like [`Window::raise`], but
    /// applied synchronously regardless of `WINDOW_HANDLING_BEHAVIOUR`.
    /// Also raises the border attached to this window, if any.
    pub fn raise_sync(self) -> eyre::Result<()> {
        WindowsApi::raise_window_sync(self.hwnd)?;
        if let Some(border_info) = crate::border_manager::window_border(self.hwnd) {
            WindowsApi::raise_window_sync(border_info.border_hwnd)?;
        }
        Ok(())
    }

    /// Raise the window above the currently active window (see
    /// [`WindowsApi::raise_window_above_active`]) without activating or focusing
    /// it, applied synchronously regardless of `WINDOW_HANDLING_BEHAVIOUR`.
    /// Also raises the border attached to this window, if any.
    pub fn raise_above_active(self) -> eyre::Result<()> {
        WindowsApi::raise_window_above_active(self.hwnd)?;
        if let Some(border_info) = crate::border_manager::window_border(self.hwnd) {
            WindowsApi::raise_window_above_active(border_info.border_hwnd)?;
        }
        Ok(())
    }

    /// Place the window into the persistent TopMost band (see
    /// [`WindowsApi::make_topmost_window`]) so it renders above every
    /// normal-band window, without activating or focusing it, applied
    /// synchronously regardless of `WINDOW_HANDLING_BEHAVIOUR`.
    /// Also raises the border attached to this window, if any.
    pub fn make_topmost(self) -> eyre::Result<()> {
        WindowsApi::make_topmost_window(self.hwnd)?;
        if let Some(border_info) = crate::border_manager::window_border(self.hwnd) {
            WindowsApi::make_topmost_window(border_info.border_hwnd)?;
        }
        Ok(())
    }

    /// Lower the window to the bottom of the Z order, but do not activate or focus
    /// it.
    /// It also checks if there is a border attached to this window and if it is
    /// it lowers it as well.
    pub fn lower(self) -> eyre::Result<()> {
        WindowsApi::lower_window(self.hwnd)?;
        if let Some(border_info) = crate::border_manager::window_border(self.hwnd) {
            WindowsApi::lower_window(border_info.border_hwnd)?;
        }
        Ok(())
    }

    /// Lower the window to the bottom of the Z order like [`Window::lower`], but
    /// applied synchronously regardless of `WINDOW_HANDLING_BEHAVIOUR`.
    /// Also lowers the border attached to this window, if any.
    pub fn lower_sync(self) -> eyre::Result<()> {
        WindowsApi::lower_window_sync(self.hwnd)?;
        if let Some(border_info) = crate::border_manager::window_border(self.hwnd) {
            WindowsApi::lower_window_sync(border_info.border_hwnd)?;
        }
        Ok(())
    }

    /// Checks whether this window looks like a regular application window
    /// (caption + window edge, not a tool/dialog frame window). Used to filter the
    /// ignored windows that are eligible to be moved by the ignored window layer.
    pub fn is_normal_application_window(self) -> bool {
        if let (Ok(style), Ok(ex_style)) = (self.style(), self.ex_style()) {
            style.contains(WindowStyle::CAPTION)
                && ex_style.contains(ExtendedWindowStyle::WINDOWEDGE)
                && !ex_style.contains(ExtendedWindowStyle::DLGMODALFRAME)
                && !ex_style.contains(ExtendedWindowStyle::TOOLWINDOW)
        } else {
            false
        }
    }

    /// Checks whether this window looks like a desktop widget or meter, i.e. a
    /// layered toolwindow popup without a caption or window edge (e.g. Rainmeter
    /// meters). Such windows are unmanaged and hover on top of the desktop, so
    /// they need to be considered by the ignored window layer to prevent them
    /// from visually occluding tiled windows.
    pub fn is_widget_window(self) -> bool {
        if let (Ok(style), Ok(ex_style)) = (self.style(), self.ex_style()) {
            !style.contains(WindowStyle::CAPTION)
                && ex_style.contains(ExtendedWindowStyle::LAYERED)
                && ex_style.contains(ExtendedWindowStyle::TOOLWINDOW)
        } else {
            false
        }
    }

    /// Checks whether this window is pinned to the topmost z-order band
    /// (WS_EX_TOPMOST), e.g. a status bar such as yasb with always_on_top
    /// enabled. Such windows are part of the ignored widget layer but should
    /// never be moved by automatic demotions.
    pub fn is_always_on_top(self) -> bool {
        self.ex_style()
            .map(|ex| ex.contains(ExtendedWindowStyle::TOPMOST))
            .unwrap_or(false)
    }

    /// Checks whether this window covers the entire monitor it is displayed on,
    /// within a small pixel tolerance. Fullscreen borderless windows (common for
    /// games) typically drop the caption and window edge styles while still
    /// covering the whole monitor, so they fail `is_normal_application_window`
    /// even though they visibly occlude the desktop. Used to include such
    /// windows in the ignored window layer.
    pub fn is_fullscreen(self) -> bool {
        let Some(monitor_rect) = self.monitor_rect() else {
            return false;
        };
        let Some(window_rect) = self.absolute_window_rect() else {
            return false;
        };

        rect_covers_monitor_rect(&window_rect, &monitor_rect, FULLSCREEN_EDGE_TOLERANCE)
    }

    /// Checks whether this window covers the whole monitor or its work area
    /// closely enough to be treated as a fullscreen coverage window by the
    /// ignored window layer. This is deliberately more lenient than
    /// `is_fullscreen`: borderless games frequently size themselves to the work
    /// area (leaving the taskbar visible) or snap to the full monitor while a
    /// few pixels short of the edge, and either way they must never be occluded
    /// by desktop widgets (e.g. Rainmeter meters).
    pub fn covers_monitor_or_work_area(self) -> bool {
        let hmonitor = HMONITOR(windows_api::as_ptr!(WindowsApi::monitor_from_window(
            self.hwnd
        )));
        let Ok(monitor_info) = WindowsApi::monitor_info_w(hmonitor) else {
            return false;
        };
        let Some(window_rect) = self.absolute_window_rect() else {
            return false;
        };

        let monitor_rect = Rect {
            left: monitor_info.monitorInfo.rcMonitor.left,
            top: monitor_info.monitorInfo.rcMonitor.top,
            right: monitor_info.monitorInfo.rcMonitor.right,
            bottom: monitor_info.monitorInfo.rcMonitor.bottom,
        };
        let work_rect = Rect {
            left: monitor_info.monitorInfo.rcWork.left,
            top: monitor_info.monitorInfo.rcWork.top,
            right: monitor_info.monitorInfo.rcWork.right,
            bottom: monitor_info.monitorInfo.rcWork.bottom,
        };

        rect_covers_target_rect(
            &window_rect,
            &monitor_rect,
            FULLSCREEN_EDGE_TOLERANCE,
            FULLSCREEN_COVERAGE_RATIO,
        ) || rect_covers_target_rect(
            &window_rect,
            &work_rect,
            FULLSCREEN_WORK_AREA_EDGE_TOLERANCE,
            FULLSCREEN_COVERAGE_RATIO,
        )
    }

    /// The physical monitor rectangle of the monitor this window is displayed
    /// on, in absolute screen coordinates.
    fn monitor_rect(self) -> Option<Rect> {
        let hmonitor = HMONITOR(windows_api::as_ptr!(WindowsApi::monitor_from_window(
            self.hwnd
        )));
        let Ok(monitor_info) = WindowsApi::monitor_info_w(hmonitor) else {
            return None;
        };
        let rc = monitor_info.monitorInfo.rcMonitor;
        Some(Rect {
            left: rc.left,
            top: rc.top,
            right: rc.right,
            bottom: rc.bottom,
        })
    }

    /// The window rectangle in absolute screen coordinates, converted from the
    /// normalized `{left, top, width, height}` form returned by
    /// `WindowsApi::window_rect`.
    fn absolute_window_rect(self) -> Option<Rect> {
        let window_rect = WindowsApi::window_rect(self.hwnd).ok()?;
        Some(Rect {
            left: window_rect.left,
            top: window_rect.top,
            right: window_rect.left + window_rect.right,
            bottom: window_rect.top + window_rect.bottom,
        })
    }

    /// Checks whether this window is in a self-managed fullscreen state: a
    /// window that covers its entire monitor while no longer looking like a
    /// normal application window (e.g. a browser with an HTML5 video in
    /// fullscreen, which drops its caption). Such windows manage their own
    /// geometry and must not be repositioned by the tiling layout or demoted
    /// by the ignored window layer, otherwise the fullscreen is interrupted.
    pub fn is_self_fullscreen(self) -> bool {
        self.is_fullscreen() && !self.is_normal_application_window()
    }

    #[tracing::instrument(fields(exe, title), skip(debug))]
    pub fn should_manage(
        self,
        event: Option<WindowManagerEvent>,
        debug: &mut RuleDebug,
    ) -> eyre::Result<bool> {
        // An explicit Manage command always takes effect, bypassing all eligibility checks
        if matches!(event, Some(WindowManagerEvent::Manage(_))) {
            debug.is_window = true;
            debug.has_minimum_width = true;
            debug.has_minimum_height = true;
            debug.has_title = true;
            debug.should_manage = true;
            return Ok(true);
        }

        if !self.is_window() {
            return Ok(false);
        }

        debug.is_window = true;

        let rect = WindowsApi::window_rect(self.hwnd).unwrap_or_default();

        if rect.right < MINIMUM_WIDTH.load(Ordering::SeqCst) {
            return Ok(false);
        }

        debug.has_minimum_width = true;

        if rect.bottom < MINIMUM_HEIGHT.load(Ordering::SeqCst) {
            return Ok(false);
        }

        debug.has_minimum_height = true;

        if self.title().is_err() {
            return Ok(false);
        }

        debug.has_title = true;

        let is_cloaked = self.is_cloaked().unwrap_or_default();

        debug.is_cloaked = is_cloaked;

        let mut allow_cloaked = false;

        if let Some(event) = event
            && matches!(
                event,
                WindowManagerEvent::Hide(_, _) | WindowManagerEvent::Cloak(_, _)
            )
        {
            allow_cloaked = true;
        }

        debug.allow_cloaked = allow_cloaked;

        match (allow_cloaked, is_cloaked) {
            // If allowing cloaked windows, we don't need to check the cloaked status
            (true, _) |
            // If not allowing cloaked windows, we need to ensure the window is not cloaked
            (false, false) => {
                if let (Ok(title), Ok(exe_name), Ok(class), Ok(path)) = (self.title(), self.exe(), self.class(), self.path()) {
                    debug.title = Some(title.clone());
                    debug.exe_name = Some(exe_name.clone());
                    debug.class = Some(class.clone());
                    debug.path = Some(path.clone());
                    // calls for styles can fail quite often for events with windows that aren't really "windows"
                    // since we have moved up calls of should_manage to the beginning of the process_event handler,
                    // we should handle failures here gracefully to be able to continue the execution of process_event
                    if let (Ok(style), Ok(ex_style)) = (&self.style(), &self.ex_style()) {
                        debug.window_style = Some(*style);
                        debug.extended_window_style = Some(*ex_style);
                        let eligible = window_is_eligible(self.hwnd, &title, &exe_name, &class, &path, style, ex_style, event, debug);
                        debug.should_manage = eligible;
                        return Ok(eligible);
                    }
                }
            }
            _ => {}
        }

        Ok(false)
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct RuleDebug {
    pub should_manage: bool,
    pub is_window: bool,
    pub has_minimum_width: bool,
    pub has_minimum_height: bool,
    pub has_title: bool,
    pub is_cloaked: bool,
    pub allow_cloaked: bool,
    pub allow_layered_transparency: bool,
    pub window_style: Option<WindowStyle>,
    pub extended_window_style: Option<ExtendedWindowStyle>,
    pub title: Option<String>,
    pub exe_name: Option<String>,
    pub class: Option<String>,
    pub path: Option<String>,
    pub matches_permaignore_class: Option<String>,
    pub matches_ignore_identifier: Option<MatchingRule>,
    pub matches_managed_override: Option<MatchingRule>,
    pub matches_layered_whitelist: Option<MatchingRule>,
    pub matches_floating_applications: Option<MatchingRule>,
    pub matches_wsl2_gui: Option<String>,
    pub matches_no_titlebar: Option<MatchingRule>,
}

#[allow(clippy::too_many_arguments)]
fn window_is_eligible(
    hwnd: isize,
    title: &String,
    exe_name: &String,
    class: &String,
    path: &str,
    style: &WindowStyle,
    ex_style: &ExtendedWindowStyle,
    event: Option<WindowManagerEvent>,
    debug: &mut RuleDebug,
) -> bool {
    {
        let permaignore_classes = PERMAIGNORE_CLASSES.lock();
        if permaignore_classes.contains(class) {
            debug.matches_permaignore_class = Some(class.clone());
            tracing::debug!(
                "unmanaged (exe: {}, title: {}, class: {}): matched permaignore class",
                exe_name,
                title,
                class,
            );
            return false;
        }
    }

    let regex_identifiers = REGEX_IDENTIFIERS.lock();

    let ignore_identifiers = IGNORE_IDENTIFIERS.lock();
    let should_ignore = if let Some(rule) = should_act(
        title,
        exe_name,
        class,
        path,
        &ignore_identifiers,
        &regex_identifiers,
    ) {
        debug.matches_ignore_identifier = Some(rule);
        true
    } else {
        false
    };

    let manage_identifiers = MANAGE_IDENTIFIERS.lock();
    let managed_override = if let Some(rule) = should_act(
        title,
        exe_name,
        class,
        path,
        &manage_identifiers,
        &regex_identifiers,
    ) {
        debug.matches_managed_override = Some(rule);
        true
    } else {
        false
    };

    let floating_identifiers = FLOATING_APPLICATIONS.lock();
    if let Some(rule) = should_act(
        title,
        exe_name,
        class,
        path,
        &floating_identifiers,
        &regex_identifiers,
    ) {
        debug.matches_floating_applications = Some(rule);
    }

    if should_ignore && !managed_override {
        tracing::debug!(
            "unmanaged (exe: {}, title: {}, class: {}): matched ignore identifier",
            exe_name,
            title,
            class,
        );
        return false;
    }

    let layered_whitelist = LAYERED_WHITELIST.lock();
    let mut allow_layered = if let Some(rule) = should_act(
        title,
        exe_name,
        class,
        path,
        &layered_whitelist,
        &regex_identifiers,
    ) {
        debug.matches_layered_whitelist = Some(rule);
        true
    } else {
        false
    };

    let known_layered_hwnds = transparency_manager::known_hwnds();

    allow_layered = if known_layered_hwnds.contains(&hwnd)
        // we always want to process hide events for windows with transparency, even on other
        // monitors, because we don't want to be left with ghost tiles
        || matches!(event, Some(WindowManagerEvent::Hide(_, _)))
    {
        debug.allow_layered_transparency = true;
        true
    } else {
        allow_layered
    };

    let allow_wsl2_gui = {
        let wsl2_ui_processes = WSL2_UI_PROCESSES.lock();
        let allow = wsl2_ui_processes.contains(exe_name);
        if allow {
            debug.matches_wsl2_gui = Some(exe_name.clone())
        }

        allow
    };

    let titlebars_removed = NO_TITLEBAR.lock();
    let allow_titlebar_removed = if let Some(rule) = should_act(
        title,
        exe_name,
        class,
        path,
        &titlebars_removed,
        &regex_identifiers,
    ) {
        debug.matches_no_titlebar = Some(rule);
        true
    } else {
        false
    };

    {
        let slow_application_identifiers = SLOW_APPLICATION_IDENTIFIERS.lock();
        let should_sleep = should_act(
            title,
            exe_name,
            class,
            path,
            &slow_application_identifiers,
            &regex_identifiers,
        )
        .is_some();

        if should_sleep {
            std::thread::sleep(Duration::from_millis(
                SLOW_APPLICATION_COMPENSATION_TIME.load(Ordering::SeqCst),
            ));
        }
    }

    if (allow_wsl2_gui || allow_titlebar_removed || style.contains(WindowStyle::CAPTION) && ex_style.contains(ExtendedWindowStyle::WINDOWEDGE))
        && !ex_style.contains(ExtendedWindowStyle::DLGMODALFRAME)
        // Get a lot of dupe events coming through that make the redrawing go crazy
        // on FocusChange events if I don't filter out this one. But, if we are
        // allowing a specific layered window on the whitelist (like Steam), it should
        // pass this check
        && (allow_layered || !ex_style.contains(ExtendedWindowStyle::LAYERED))
        || managed_override
    {
        return true;
    } else {
        tracing::debug!(
            "unmanaged (exe: {}, title: {}, class: {}): does not meet window eligibility criteria (no WS_CAPTION + WS_EX_WINDOWEDGE, or layered without whitelist)",
            exe_name,
            title,
            class,
        );
    }

    false
}

#[allow(clippy::cognitive_complexity, clippy::too_many_lines)]
pub fn should_act(
    title: &str,
    exe_name: &str,
    class: &str,
    path: &str,
    identifiers: &[MatchingRule],
    regex_identifiers: &HashMap<String, Regex>,
) -> Option<MatchingRule> {
    let mut matching_rule = None;
    for rule in identifiers {
        match rule {
            MatchingRule::Simple(identifier) => {
                if should_act_individual(
                    title,
                    exe_name,
                    class,
                    path,
                    identifier,
                    regex_identifiers,
                ) {
                    matching_rule = Some(rule.clone());
                };
            }
            MatchingRule::Composite(identifiers) => {
                let mut composite_results = vec![];
                for identifier in identifiers {
                    composite_results.push(should_act_individual(
                        title,
                        exe_name,
                        class,
                        path,
                        identifier,
                        regex_identifiers,
                    ));
                }

                if composite_results.iter().all(|&x| x) {
                    matching_rule = Some(rule.clone());
                }
            }
        }
    }

    matching_rule
}

pub fn should_act_individual(
    title: &str,
    exe_name: &str,
    class: &str,
    path: &str,
    identifier: &IdWithIdentifier,
    regex_identifiers: &HashMap<String, Regex>,
) -> bool {
    let mut should_act = false;

    match identifier.matching_strategy {
        None | Some(MatchingStrategy::Legacy) => match identifier.kind {
            ApplicationIdentifier::Title => {
                if title.starts_with(&identifier.id) || title.ends_with(&identifier.id) {
                    should_act = true;
                }
            }
            ApplicationIdentifier::Class => {
                if class.starts_with(&identifier.id) || class.ends_with(&identifier.id) {
                    should_act = true;
                }
            }
            ApplicationIdentifier::Exe => {
                if exe_name.eq(&identifier.id) {
                    should_act = true;
                }
            }
            ApplicationIdentifier::Path => {
                if path.eq(&identifier.id) {
                    should_act = true;
                }
            }
        },
        Some(MatchingStrategy::Equals) => match identifier.kind {
            ApplicationIdentifier::Title => {
                if title.eq(&identifier.id) {
                    should_act = true;
                }
            }
            ApplicationIdentifier::Class => {
                if class.eq(&identifier.id) {
                    should_act = true;
                }
            }
            ApplicationIdentifier::Exe => {
                if exe_name.eq(&identifier.id) {
                    should_act = true;
                }
            }
            ApplicationIdentifier::Path => {
                if path.eq(&identifier.id) {
                    should_act = true;
                }
            }
        },
        Some(MatchingStrategy::DoesNotEqual) => match identifier.kind {
            ApplicationIdentifier::Title => {
                if !title.eq(&identifier.id) {
                    should_act = true;
                }
            }
            ApplicationIdentifier::Class => {
                if !class.eq(&identifier.id) {
                    should_act = true;
                }
            }
            ApplicationIdentifier::Exe => {
                if !exe_name.eq(&identifier.id) {
                    should_act = true;
                }
            }
            ApplicationIdentifier::Path => {
                if !path.eq(&identifier.id) {
                    should_act = true;
                }
            }
        },
        Some(MatchingStrategy::StartsWith) => match identifier.kind {
            ApplicationIdentifier::Title => {
                if title.starts_with(&identifier.id) {
                    should_act = true;
                }
            }
            ApplicationIdentifier::Class => {
                if class.starts_with(&identifier.id) {
                    should_act = true;
                }
            }
            ApplicationIdentifier::Exe => {
                if exe_name.starts_with(&identifier.id) {
                    should_act = true;
                }
            }
            ApplicationIdentifier::Path => {
                if path.starts_with(&identifier.id) {
                    should_act = true;
                }
            }
        },
        Some(MatchingStrategy::DoesNotStartWith) => match identifier.kind {
            ApplicationIdentifier::Title => {
                if !title.starts_with(&identifier.id) {
                    should_act = true;
                }
            }
            ApplicationIdentifier::Class => {
                if !class.starts_with(&identifier.id) {
                    should_act = true;
                }
            }
            ApplicationIdentifier::Exe => {
                if !exe_name.starts_with(&identifier.id) {
                    should_act = true;
                }
            }
            ApplicationIdentifier::Path => {
                if !path.starts_with(&identifier.id) {
                    should_act = true;
                }
            }
        },
        Some(MatchingStrategy::EndsWith) => match identifier.kind {
            ApplicationIdentifier::Title => {
                if title.ends_with(&identifier.id) {
                    should_act = true;
                }
            }
            ApplicationIdentifier::Class => {
                if class.ends_with(&identifier.id) {
                    should_act = true;
                }
            }
            ApplicationIdentifier::Exe => {
                if exe_name.ends_with(&identifier.id) {
                    should_act = true;
                }
            }
            ApplicationIdentifier::Path => {
                if path.ends_with(&identifier.id) {
                    should_act = true;
                }
            }
        },
        Some(MatchingStrategy::DoesNotEndWith) => match identifier.kind {
            ApplicationIdentifier::Title => {
                if !title.ends_with(&identifier.id) {
                    should_act = true;
                }
            }
            ApplicationIdentifier::Class => {
                if !class.ends_with(&identifier.id) {
                    should_act = true;
                }
            }
            ApplicationIdentifier::Exe => {
                if !exe_name.ends_with(&identifier.id) {
                    should_act = true;
                }
            }
            ApplicationIdentifier::Path => {
                if !path.ends_with(&identifier.id) {
                    should_act = true;
                }
            }
        },
        Some(MatchingStrategy::Contains) => match identifier.kind {
            ApplicationIdentifier::Title => {
                if title.contains(&identifier.id) {
                    should_act = true;
                }
            }
            ApplicationIdentifier::Class => {
                if class.contains(&identifier.id) {
                    should_act = true;
                }
            }
            ApplicationIdentifier::Exe => {
                if exe_name.contains(&identifier.id) {
                    should_act = true;
                }
            }
            ApplicationIdentifier::Path => {
                if path.contains(&identifier.id) {
                    should_act = true;
                }
            }
        },
        Some(MatchingStrategy::DoesNotContain) => match identifier.kind {
            ApplicationIdentifier::Title => {
                if !title.contains(&identifier.id) {
                    should_act = true;
                }
            }
            ApplicationIdentifier::Class => {
                if !class.contains(&identifier.id) {
                    should_act = true;
                }
            }
            ApplicationIdentifier::Exe => {
                if !exe_name.contains(&identifier.id) {
                    should_act = true;
                }
            }
            ApplicationIdentifier::Path => {
                if !path.contains(&identifier.id) {
                    should_act = true;
                }
            }
        },
        Some(MatchingStrategy::Regex) => match identifier.kind {
            ApplicationIdentifier::Title => {
                if let Some(re) = regex_identifiers.get(&identifier.id)
                    && re.is_match(title)
                {
                    should_act = true;
                }
            }
            ApplicationIdentifier::Class => {
                if let Some(re) = regex_identifiers.get(&identifier.id)
                    && re.is_match(class)
                {
                    should_act = true;
                }
            }
            ApplicationIdentifier::Exe => {
                if let Some(re) = regex_identifiers.get(&identifier.id)
                    && re.is_match(exe_name)
                {
                    should_act = true;
                }
            }
            ApplicationIdentifier::Path => {
                if let Some(re) = regex_identifiers.get(&identifier.id)
                    && re.is_match(path)
                {
                    should_act = true;
                }
            }
        },
    }

    should_act
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(left: i32, top: i32, right: i32, bottom: i32) -> Rect {
        Rect { left, top, right, bottom }
    }

    #[test]
    fn test_rect_covers_monitor_rect_when_window_matches_monitor() {
        let monitor = rect(0, 0, 1920, 1080);

        assert!(rect_covers_monitor_rect(&monitor, &monitor, 0));
        assert!(rect_covers_monitor_rect(&rect(0, 0, 1920, 1080), &monitor, FULLSCREEN_EDGE_TOLERANCE));

        // Edge coordinate slightly inside the monitor still counts as coverage.
        assert!(rect_covers_monitor_rect(&rect(0, 0, 1916, 1080), &monitor, 8));
    }

    #[test]
    fn test_rect_covers_monitor_rect_when_window_does_not_match() {
        let monitor = rect(0, 0, 1920, 1080);

        assert!(!rect_covers_monitor_rect(&rect(10, 10, 1910, 1070), &monitor, 8));
        assert!(!rect_covers_monitor_rect(&rect(0, 0, 1920, 500), &monitor, 8));
        assert!(!rect_covers_monitor_rect(&rect(0, 0, 1920, 1050), &monitor, 8));
    }

    #[test]
    fn test_rect_covers_monitor_rect_on_secondary_monitor() {
        // A monitor whose origin is not (0, 0); the window rectangle is given in
        // absolute screen coordinates.
        let monitor = rect(1280, 0, 3840, 1440);

        assert!(rect_covers_monitor_rect(&rect(1280, 0, 3840, 1440), &monitor, 8));
        assert!(rect_covers_monitor_rect(&rect(1280, 0, 3832, 1440), &monitor, 8));
        assert!(!rect_covers_monitor_rect(&rect(1280, 0, 3830, 1440), &monitor, 8));
    }

    #[test]
    fn test_rect_covers_target_rect_by_area_fallback() {
        let monitor = rect(0, 0, 2560, 1440);

        // A work-area-sized window (2560x1410, taskbar visible) is 97.9% of the
        // monitor area: not edge-covered, but area-covered.
        assert!(rect_covers_target_rect(
            &rect(0, 0, 2560, 1410),
            &monitor,
            FULLSCREEN_EDGE_TOLERANCE,
            FULLSCREEN_COVERAGE_RATIO,
        ));

        // A window spanning two monitors still covers the whole first monitor.
        assert!(rect_covers_target_rect(
            &rect(0, 0, 5120, 1440),
            &monitor,
            FULLSCREEN_EDGE_TOLERANCE,
            FULLSCREEN_COVERAGE_RATIO,
        ));

        // A desktop widget (617x142) is nowhere near fullscreen coverage.
        assert!(!rect_covers_target_rect(
            &rect(0, 0, 617, 142),
            &monitor,
            FULLSCREEN_EDGE_TOLERANCE,
            FULLSCREEN_COVERAGE_RATIO,
        ));
    }

    #[test]
    fn test_rect_covers_target_rect_by_work_area() {
        let work_area = rect(0, 0, 2560, 1410);

        assert!(rect_covers_target_rect(
            &rect(0, 0, 2560, 1410),
            &work_area,
            FULLSCREEN_WORK_AREA_EDGE_TOLERANCE,
            FULLSCREEN_COVERAGE_RATIO,
        ));
        assert!(rect_covers_target_rect(
            &rect(0, 0, 2554, 1404),
            &work_area,
            FULLSCREEN_WORK_AREA_EDGE_TOLERANCE,
            FULLSCREEN_COVERAGE_RATIO,
        ));
        assert!(!rect_covers_target_rect(
            &rect(0, 0, 617, 142),
            &work_area,
            FULLSCREEN_WORK_AREA_EDGE_TOLERANCE,
            FULLSCREEN_COVERAGE_RATIO,
        ));
    }
}
