//! Menu-bar indicator for whether the speech model is loaded.
//!
//! The tray is built in `lib.rs` with the bundle icon and `icon_as_template(true)`.
//! When the model is unloaded we swap in a hollow rounded-square template icon so the
//! menu bar shows an empty outline; when loaded we restore the bundle icon.

use tauri::AppHandle;

const ICON_SIZE: u32 = 36;
const ICON_MARGIN: f64 = 2.0;
const CORNER_RADIUS: f64 = 8.0;
const RING_THICKNESS: f64 = 3.0;

/// Swap the tray icon and tooltip to reflect the current model state.
///
/// Errors are deliberately ignored: the tray is cosmetic and must never take down the
/// app if the platform refuses an update.
pub fn update_model_indicator(app: &AppHandle, loaded: bool) {
    let Some(tray) = app.tray_by_id("voxtype") else {
        return;
    };

    let icon = if loaded {
        app.default_window_icon().cloned()
    } else {
        let (rgba, width, height) = unloaded_icon_rgba();
        Some(tauri::image::Image::new_owned(rgba, width, height))
    };

    let _ = tray.set_icon(icon);
    // `TrayIcon::set_icon` renders with template = false, so re-assert the template
    // flag after every icon swap to keep the monochrome menu-bar treatment.
    let _ = tray.set_icon_as_template(true);
    let tooltip = if loaded {
        "VoxType - model loaded"
    } else {
        "VoxType - model not loaded"
    };
    let _ = tray.set_tooltip(Some(tooltip));
}

/// Draw a hollow rounded-square outline as transparent RGBA8.
///
/// Returns `(rgba, width, height)` ready for [`tauri::image::Image::new_owned`].
fn unloaded_icon_rgba() -> (Vec<u8>, u32, u32) {
    let size = ICON_SIZE as usize;
    let mut rgba = vec![0u8; size * size * 4];

    let outer_left = ICON_MARGIN;
    let outer_top = ICON_MARGIN;
    let outer_right = ICON_SIZE as f64 - ICON_MARGIN;
    let outer_bottom = ICON_SIZE as f64 - ICON_MARGIN;

    let inner_left = outer_left + RING_THICKNESS;
    let inner_top = outer_top + RING_THICKNESS;
    let inner_right = outer_right - RING_THICKNESS;
    let inner_bottom = outer_bottom - RING_THICKNESS;
    let inner_radius = CORNER_RADIUS - RING_THICKNESS;

    for (index, pixel) in rgba.chunks_exact_mut(4).enumerate() {
        let center_x = (index % size) as f64 + 0.5;
        let center_y = (index / size) as f64 + 0.5;
        let in_outer = inside_rounded_rect(
            center_x,
            center_y,
            outer_left,
            outer_top,
            outer_right,
            outer_bottom,
            CORNER_RADIUS,
        );
        let in_inner = inside_rounded_rect(
            center_x,
            center_y,
            inner_left,
            inner_top,
            inner_right,
            inner_bottom,
            inner_radius,
        );
        if in_outer && !in_inner {
            // Ring pixels are opaque black; the buffer already zeroes RGB and alpha.
            pixel[3] = 255;
        }
    }

    (rgba, ICON_SIZE, ICON_SIZE)
}

/// Signed-distance test for a rounded rectangle, using pixel-center coordinates.
fn inside_rounded_rect(
    px: f64,
    py: f64,
    left: f64,
    top: f64,
    right: f64,
    bottom: f64,
    radius: f64,
) -> bool {
    let nearest_x = px.clamp(left + radius, right - radius);
    let nearest_y = py.clamp(top + radius, bottom - radius);
    let dx = px - nearest_x;
    let dy = py - nearest_y;
    dx * dx + dy * dy <= radius * radius
}

#[cfg(test)]
mod tests {
    use super::*;

    fn alpha_at(rgba: &[u8], width: u32, x: u32, y: u32) -> u8 {
        rgba[((y * width + x) * 4 + 3) as usize]
    }

    #[test]
    fn icon_buffer_matches_dimensions() {
        let (rgba, width, height) = unloaded_icon_rgba();
        assert_eq!(rgba.len(), (width * height * 4) as usize);
    }

    #[test]
    fn center_pixel_is_transparent() {
        let (rgba, width, _height) = unloaded_icon_rgba();
        assert_eq!(alpha_at(&rgba, width, width / 2, width / 2), 0);
    }

    #[test]
    fn top_edge_ring_is_opaque() {
        let (rgba, width, _height) = unloaded_icon_rgba();
        // Middle of the top edge, inside the 3 px-thick ring.
        assert_eq!(alpha_at(&rgba, width, width / 2, 3), 255);
    }

    #[test]
    fn rounded_corner_is_transparent() {
        let (rgba, width, _height) = unloaded_icon_rgba();
        assert_eq!(alpha_at(&rgba, width, 0, 0), 0);
    }
}
