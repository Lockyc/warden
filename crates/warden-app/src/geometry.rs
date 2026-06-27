use crate::surface::PixelRect;

/// A rect as the web layer sees it: CSS pixels, origin top-left of the webview.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WebRect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

/// Convert a web rect (top-left origin, CSS px) to an AppKit view rect
/// (bottom-left origin, points). `view_height_pts` is the content view height in points.
/// Scale is applied separately via `backing_size`; this function works in points only.
pub fn web_rect_to_view(web: WebRect, view_height_pts: f64) -> PixelRect {
    PixelRect {
        x: web.x,
        y: view_height_pts - web.y - web.height, // flip Y
        width: web.width,
        height: web.height,
    }
}

/// Convert a point-coordinate rect to a backing (framebuffer) pixel size at the
/// given scale factor. The floor of 1.0 ensures the surface is never zero-sized.
pub fn backing_size(rect: PixelRect, scale: f64) -> (u32, u32) {
    (
        (rect.width * scale).max(1.0) as u32,
        (rect.height * scale).max(1.0) as u32,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flips_y_origin() {
        // A 100×40 web rect at top-left (10, 20) in a 600pt-tall view.
        let got = web_rect_to_view(
            WebRect { x: 10.0, y: 20.0, width: 100.0, height: 40.0 },
            600.0,
        );
        assert_eq!(
            got,
            PixelRect { x: 10.0, y: 540.0, width: 100.0, height: 40.0 }
        );
    }

    #[test]
    fn full_height_rect_lands_at_origin() {
        let got = web_rect_to_view(
            WebRect { x: 0.0, y: 0.0, width: 900.0, height: 600.0 },
            600.0,
        );
        assert_eq!(got, PixelRect { x: 0.0, y: 0.0, width: 900.0, height: 600.0 });
    }

    #[test]
    fn backing_size_scales_up() {
        // 100×50 pt at 2× DPI → 200×100 px
        let rect = PixelRect { x: 0.0, y: 0.0, width: 100.0, height: 50.0 };
        assert_eq!(backing_size(rect, 2.0), (200, 100));
    }

    #[test]
    fn backing_size_identity_at_1x() {
        let rect = PixelRect { x: 0.0, y: 0.0, width: 300.0, height: 150.0 };
        assert_eq!(backing_size(rect, 1.0), (300, 150));
    }

    #[test]
    fn backing_size_zero_rect_floors_to_one() {
        // Zero-size rect must not produce a zero backing — libghostty rejects it.
        let rect = PixelRect { x: 0.0, y: 0.0, width: 0.0, height: 0.0 };
        assert_eq!(backing_size(rect, 2.0), (1, 1));
    }
}
