//! `tabs.captureVisibleTab` and Firefox's `tabs.captureTab`: a picture of a
//! tab the extension may read, as a PNG data URL. WebKit draws the snapshot;
//! a rectangle crops it from the whole document and a scale resizes it.

use base64::Engine as _;
use serde_json::{json, Value};
use webkit2gtk::{SnapshotOptions, SnapshotRegion, WebViewExt};
use zephium_core::ids::ProfileId;

use super::routing::{error, ok};
use crate::platform::imp::extension_channel::Message;

/// More pixels than this is refused rather than drawn: about 8K by 8K.
const MAX_PIXELS: f64 = 64_000_000.0;

/// What to draw: the part of the document, in CSS pixels, and the scale.
#[derive(Debug, PartialEq)]
struct Shot {
    rect: Option<(f64, f64, f64, f64)>,
    scale: f64,
}

/// The options of either call. `captureVisibleTab` takes no rectangle.
fn shot(options: &Value) -> Result<Shot, String> {
    let rect = match &options["rect"] {
        Value::Object(rect) => {
            let number = |key: &str| rect.get(key).and_then(Value::as_f64).filter(|value| value.is_finite());
            let (x, y, width, height) = (number("x"), number("y"), number("width"), number("height"));
            match (x, y, width, height) {
                (Some(x), Some(y), Some(width), Some(height)) if width > 0.0 && height > 0.0 => Some((x, y, width, height)),
                _ => return Err("The capture rectangle is not valid.".into()),
            }
        }
        _ => None,
    };
    let scale = options["scale"].as_f64().filter(|scale| scale.is_finite() && *scale > 0.0).unwrap_or(1.0).min(4.0);
    if let Some((_, _, width, height)) = rect {
        if width * height * scale * scale > MAX_PIXELS {
            return Err("The capture is larger than the browser draws.".into());
        }
    }
    Ok(Shot { rect, scale })
}

impl super::super::EngineHost {
    /// Answers `message` once WebKit has drawn the tab.
    pub(super) fn linux_capture(&mut self, profile: ProfileId, message: Message, id: u64, api: &str, args: &[Value]) {
        // captureVisibleTab([windowId], [options]); captureTab([tabId], [options]).
        let (target, options) = match args {
            [Value::Number(number), options, ..] => (number.as_i64(), options.clone()),
            [Value::Number(number)] => (number.as_i64(), Value::Null),
            [Value::Null, options, ..] => (None, options.clone()),
            [options, ..] => (None, options.clone()),
            [] => (None, Value::Null),
        };
        // A window ID names the window, not a tab: its active tab is drawn.
        let tab = if api == "tabs.captureTab" { target } else { None };
        let prepared = shot(&options).and_then(|shot| Ok((self.linux_readable_tab(profile, &message.extension_id, tab)?, shot)));
        let (view, shot) = match prepared {
            Ok(prepared) => prepared,
            Err(reason) => return message.reply(&error(id, &reason)),
        };
        let region = if shot.rect.is_some() { SnapshotRegion::FullDocument } else { SnapshotRegion::Visible };
        view.snapshot(region, SnapshotOptions::NONE, None::<&gtk::gio::Cancellable>, move |surface| {
            let picture = surface.map_err(|failure| failure.to_string()).and_then(|surface| encode(&surface, &shot));
            match picture {
                Ok(url) => message.reply(&ok(id, json!(url))),
                Err(reason) => message.reply(&error(id, &format!("The tab could not be captured: {reason}"))),
            }
        });
    }
}

/// The snapshot cropped and scaled as asked, as a PNG data URL.
fn encode(surface: &gtk::cairo::Surface, shot: &Shot) -> Result<String, String> {
    let image = gtk::cairo::ImageSurface::try_from(surface.clone()).map_err(|_| "WebKit drew no image")?;
    let (full_width, full_height) = (image.width(), image.height());
    let (x, y, width, height) = match shot.rect {
        Some((x, y, width, height)) => {
            let x = (x.max(0.0) as i32).min(full_width);
            let y = (y.max(0.0) as i32).min(full_height);
            (x, y, (width as i32).min(full_width - x), (height as i32).min(full_height - y))
        }
        None => (0, 0, full_width, full_height),
    };
    if width <= 0 || height <= 0 {
        return Err("the rectangle lies outside the page".into());
    }
    let mut picture = gtk::gdk::pixbuf_get_from_surface(&image, x, y, width, height).ok_or("the snapshot could not be read")?;
    if (shot.scale - 1.0).abs() > f64::EPSILON {
        let (scaled_width, scaled_height) = (((width as f64) * shot.scale).round() as i32, ((height as f64) * shot.scale).round() as i32);
        picture = picture
            .scale_simple(scaled_width.max(1), scaled_height.max(1), gtk::gdk_pixbuf::InterpType::Bilinear)
            .ok_or("the snapshot could not be scaled")?;
    }
    let png = picture.save_to_bufferv("png", &[]).map_err(|failure| failure.to_string())?;
    Ok(format!("data:image/png;base64,{}", base64::engine::general_purpose::STANDARD.encode(png)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn options_read_as_chrome_and_firefox_give_them() {
        assert_eq!(shot(&Value::Null).unwrap(), Shot { rect: None, scale: 1.0 });
        let firefox = json!({ "format": "png", "rect": { "x": 0, "y": 10, "width": 300, "height": 200 }, "scale": 2 });
        assert_eq!(shot(&firefox).unwrap(), Shot { rect: Some((0.0, 10.0, 300.0, 200.0)), scale: 2.0 });
        assert!(shot(&json!({ "rect": { "x": 0, "y": 0, "width": 0, "height": 5 } })).is_err());
        assert!(shot(&json!({ "rect": { "x": 0, "y": 0, "width": 20000, "height": 20000 } })).is_err());
    }
}
