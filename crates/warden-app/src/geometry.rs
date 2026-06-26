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
/// `scale` is the backing scale factor (libghostty wants pixel size separately; here we
/// keep points for the NSView frame and let the caller pass scale to ghostty_surface_set_content_scale).
pub fn web_rect_to_view(web: WebRect, view_height_pts: f64, _scale: f64) -> PixelRect {
    PixelRect {
        x: web.x,
        y: view_height_pts - web.y - web.height, // flip Y
        width: web.width,
        height: web.height,
    }
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
            2.0,
        );
        assert_eq!(
            got,
            PixelRect { x: 10.0, y: 600.0 - 20.0 - 40.0, width: 100.0, height: 40.0 }
        );
    }

    #[test]
    fn full_height_rect_lands_at_origin() {
        let got = web_rect_to_view(
            WebRect { x: 0.0, y: 0.0, width: 900.0, height: 600.0 },
            600.0,
            1.0,
        );
        assert_eq!(got, PixelRect { x: 0.0, y: 0.0, width: 900.0, height: 600.0 });
    }
}
