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

/// Snap a view rect INWARD to the device-pixel grid at `scale`: the origin rounds UP, the far
/// edges round DOWN, so the snapped rect never extends past the reported one. Two reasons a
/// surface frame must land on whole device pixels: a CALayer at a fractional origin is
/// resampled by the compositor (soft terminal text), and — the one that shipped — a pane's
/// divider edge sits wherever the flex ratio puts it (`0.3 × 1255px = 376.5px`), so a frame
/// rounded to NEAREST can spill half a pixel outward over the focus ring's own column, which
/// the webview paints pixel-snapped: at exactly the window widths that put the edge on a
/// half-pixel the ring vanished along the divider. Inward is the only direction that can't
/// cover the ring; the cost is at most one device pixel of ground on that side, invisible
/// against the pane's own ground. The web side stays the layout authority — this only chooses
/// which pixels of what it reported the surface may occupy. The `EPS` guards float noise on a
/// value that is meant to be integral (`813.0000001` must not lose a whole pixel to `ceil`).
pub fn snap_inward(rect: PixelRect, scale: f64) -> PixelRect {
    const EPS: f64 = 1e-6;
    let s = if scale.is_finite() && scale > 0.0 {
        scale
    } else {
        1.0
    };
    let x0 = (rect.x * s - EPS).ceil() / s;
    let y0 = (rect.y * s - EPS).ceil() / s;
    let x1 = ((rect.x + rect.width) * s + EPS).floor() / s;
    let y1 = ((rect.y + rect.height) * s + EPS).floor() / s;
    PixelRect {
        x: x0,
        y: y0,
        width: (x1 - x0).max(0.0),
        height: (y1 - y0).max(0.0),
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
            WebRect {
                x: 10.0,
                y: 20.0,
                width: 100.0,
                height: 40.0,
            },
            600.0,
        );
        assert_eq!(
            got,
            PixelRect {
                x: 10.0,
                y: 540.0,
                width: 100.0,
                height: 40.0
            }
        );
    }

    #[test]
    fn full_height_rect_lands_at_origin() {
        let got = web_rect_to_view(
            WebRect {
                x: 0.0,
                y: 0.0,
                width: 900.0,
                height: 600.0,
            },
            600.0,
        );
        assert_eq!(
            got,
            PixelRect {
                x: 0.0,
                y: 0.0,
                width: 900.0,
                height: 600.0
            }
        );
    }

    #[test]
    fn snap_inward_rounds_origin_up_and_far_edge_down() {
        // The shipped case: a divider edge on a half pixel at 1×. x 621.5..998 → 622..998.
        let got = snap_inward(
            PixelRect {
                x: 621.5,
                y: 0.0,
                width: 376.5,
                height: 1000.0,
            },
            1.0,
        );
        assert_eq!(
            got,
            PixelRect {
                x: 622.0,
                y: 0.0,
                width: 376.0,
                height: 1000.0
            }
        );
    }

    #[test]
    fn snap_inward_uses_the_device_pixel_grid_at_2x() {
        // At 2× a half-point IS on the grid; a quarter-point is not and snaps up to the half.
        let got = snap_inward(
            PixelRect {
                x: 10.25,
                y: 0.5,
                width: 100.5,
                height: 50.0,
            },
            2.0,
        );
        assert_eq!(
            got,
            PixelRect {
                x: 10.5,
                y: 0.5,
                width: 100.0, // far edge 110.75 → 110.5
                height: 50.0
            }
        );
    }

    #[test]
    fn snap_inward_leaves_an_integral_rect_alone_despite_float_noise() {
        let got = snap_inward(
            PixelRect {
                x: 813.0 + 1e-9,
                y: 0.0,
                width: 100.0 - 2e-9,
                height: 40.0,
            },
            1.0,
        );
        assert_eq!(
            got,
            PixelRect {
                x: 813.0,
                y: 0.0,
                width: 100.0,
                height: 40.0
            }
        );
    }

    #[test]
    fn snap_inward_never_goes_negative() {
        // A sub-pixel sliver collapses to zero width, not a negative one.
        let got = snap_inward(
            PixelRect {
                x: 0.4,
                y: 0.0,
                width: 0.3,
                height: 1.0,
            },
            1.0,
        );
        assert_eq!(got.width, 0.0);
        assert_eq!(got.x, 1.0);
    }

    #[test]
    fn backing_size_scales_up() {
        // 100×50 pt at 2× DPI → 200×100 px
        let rect = PixelRect {
            x: 0.0,
            y: 0.0,
            width: 100.0,
            height: 50.0,
        };
        assert_eq!(backing_size(rect, 2.0), (200, 100));
    }

    #[test]
    fn backing_size_identity_at_1x() {
        let rect = PixelRect {
            x: 0.0,
            y: 0.0,
            width: 300.0,
            height: 150.0,
        };
        assert_eq!(backing_size(rect, 1.0), (300, 150));
    }

    #[test]
    fn backing_size_zero_rect_floors_to_one() {
        // Zero-size rect must not produce a zero backing — libghostty rejects it.
        let rect = PixelRect {
            x: 0.0,
            y: 0.0,
            width: 0.0,
            height: 0.0,
        };
        assert_eq!(backing_size(rect, 2.0), (1, 1));
    }
}
