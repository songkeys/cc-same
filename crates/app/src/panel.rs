//! The window as a panel under the menu bar icon (macOS). A click on the icon opens it there, like
//! a menu; it closes once something else is clicked. The icon's menu moves to a right-click.

use gpui_kit::{App, Bounds, Pixels, Point, WindowBounds, WindowKind, WindowOptions, point, px, size};
use std::time::{Duration, Instant};

/// The panel's size; it is never taller than the screen below the menu bar.
const WIDTH: f32 = 400.;
const HEIGHT: f32 = 600.;
/// Room between the menu bar and the panel, and between the panel and the screen's edge.
const GAP: f32 = 6.;
/// The menu bar's height at most (taller beside a camera notch), in points.
#[cfg(target_os = "macos")]
const MENU_BAR: f64 = 50.;

/// Where the panel hangs from: the middle of the icon's bottom edge, in points from the top left
/// of the main screen.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Anchor {
    pub x: f64,
    pub y: f64,
}

/// The anchor under an icon at `x`, `y`, `width` by `height` physical pixels from the top left
/// of the main screen, as the tray reports it, read with the scale of the screen it is on.
#[cfg(target_os = "macos")]
pub fn below(x: f64, y: f64, width: f64, height: f64) -> Option<Anchor> {
    use objc2::MainThreadMarker;
    use objc2_app_kit::NSScreen;

    let mtm = MainThreadMarker::new()?;
    let screens = NSScreen::screens(mtm);
    // Screen frames count up from the bottom of the main screen, the first one.
    let main_height = screens.firstObject()?.frame().size.height;
    screens.iter().find_map(|screen| {
        let scale = screen.backingScaleFactor();
        let frame = screen.frame();
        let (left, top) = (frame.origin.x, main_height - frame.origin.y - frame.size.height);
        let (x, y) = (x / scale, y / scale);
        // An icon not placed yet (just after launch) reports somewhere else: only the menu bar,
        // along the top of a screen, counts.
        let in_menu_bar = (left..=left + frame.size.width).contains(&x) && (top..=top + MENU_BAR).contains(&y);
        in_menu_bar.then(|| Anchor { x: x + width / scale / 2., y: y + height / scale })
    })
}

#[cfg(not(target_os = "macos"))]
pub fn below(_x: f64, _y: f64, _width: f64, _height: f64) -> Option<Anchor> {
    None
}

/// The panel's window: borderless, above other windows, centred under `anchor` (or at the top
/// right of the main screen), and kept on the screen.
pub fn options(anchor: Option<Anchor>, cx: &App) -> WindowOptions {
    let at: Point<Pixels> = match anchor {
        Some(a) => point(px(a.x as f32), px(a.y as f32)),
        None => {
            let screen = cx.primary_display().map(|d| d.bounds());
            let right = screen.map_or(px(1440.), |b| b.origin.x + b.size.width);
            point(right - px(WIDTH / 2. + GAP * 4.), px(30.))
        }
    };
    let display = cx.displays().into_iter().find(|d| d.bounds().contains(&at)).or_else(|| cx.primary_display());
    let screen = display.as_ref().map(|d| d.bounds());
    // GPUI places a window relative to its display.
    let (left, top, screen_width, screen_height) =
        screen.map_or((px(0.), px(0.), px(1440.), px(900.)), |b| (b.origin.x, b.origin.y, b.size.width, b.size.height));
    let height = px(HEIGHT).min(screen_height - (at.y - top) - px(GAP * 2.)).max(px(360.));
    let x = (at.x - left - px(WIDTH / 2.)).clamp(px(GAP), screen_width - px(WIDTH + GAP));
    let y = at.y - top + px(GAP);
    WindowOptions {
        titlebar: None,
        kind: WindowKind::PopUp,
        is_movable: false,
        is_resizable: false,
        app_id: Some(crate::APP_ID.into()),
        display_id: display.map(|d| d.id()),
        window_bounds: Some(WindowBounds::Windowed(Bounds { origin: point(x, y), size: size(px(WIDTH), height) })),
        ..WindowOptions::default()
    }
}

/// When the panel last closed because something else was clicked.
#[derive(Default)]
pub struct Closed(Option<Instant>);

impl gpui_kit::Global for Closed {}

/// Note that the panel just closed by itself.
pub fn closed(cx: &mut App) {
    cx.set_global(Closed(Some(Instant::now())));
}

/// A click on the icon while the panel is open closes it before the click arrives: that click is
/// the one that closed it, not one to open it again.
pub fn just_closed(cx: &App) -> bool {
    cx.try_global::<Closed>().and_then(|c| c.0).is_some_and(|at| at.elapsed() < Duration::from_millis(400))
}
