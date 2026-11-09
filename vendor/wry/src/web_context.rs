// Copyright 2020-2023 Tauri Programme within The Commons Conservancy
// SPDX-License-Identifier: Apache-2.0
// SPDX-License-Identifier: MIT

#[cfg(gtk)]
use crate::webkitgtk::WebContextImpl;

use std::{
  collections::HashSet,
  path::{Path, PathBuf},
};

/// A context that is shared between multiple [`WebView`]s.
///
/// A browser would have a context for all the normal tabs and a different context for all the
/// private/incognito tabs.
///
/// # Warning
///
/// If [`WebView`] is created by a WebContext. Dropping `WebContext` will cause [`WebView`] lose
/// some actions like custom protocol on Mac. Please keep both instances when you still wish to
/// interact with them.
///
/// [`WebView`]: crate::WebView
#[derive(Debug)]
pub struct WebContext {
  data_directory: Option<PathBuf>,
  #[allow(dead_code)] // It's not needed on Windows and macOS.
  pub(crate) os: WebContextImpl,
  #[allow(dead_code)] // It's not needed on Windows and macOS.
  pub(crate) custom_protocols: HashSet<String>,
}

impl WebContext {
  /// Create a new [`WebContext`].
  ///
  /// - `data_directory`: Whether the WebView window should have a custom user data path.
  ///   This is useful in Windows when a bundled application can't have the webview data inside `Program Files`.
  ///
  /// ## Platform-specific:
  ///
  /// - **Linux / BSD**: `None` creates an ephemeral native context in this
  ///   fork. Persistent storage requires an explicit profile data directory;
  ///   an unspecified platform-default persistence location is never used.
  /// - **Windows**: Webview instances with different `CoreWebView2EnvironmentOptions` must have different `data_directory`s [^1]
  ///
  /// [^1]: <https://learn.microsoft.com/en-us/dotnet/api/microsoft.web.webview2.core.corewebview2environment.createcorewebview2controllerasync?view=webview2-dotnet-1.0.3719.77#:~:text=WebView%20creation%20fails%20if%20a%20running%20instance%20using%20the%20same%20user%20data%20folder%20exists%2C%20and%20the%20Environment%20objects%20have%20different%20CoreWebView2EnvironmentOptions.>
  pub fn new(data_directory: Option<PathBuf>) -> Self {
    Self {
      os: WebContextImpl::new(data_directory.as_deref()),
      data_directory,
      custom_protocols: Default::default(),
    }
  }

  /// Creates a GTK context only when sandbox and cross-site process-swap
  /// postconditions can be read back successfully. `new` remains
  /// source-compatible for existing embedders such as tauri-runtime-wry; an
  /// unverified context is still rejected by `WebViewBuilder::build_gtk`
  /// before it can create a WebView.
  #[cfg(gtk)]
  pub fn try_new(data_directory: Option<PathBuf>) -> crate::Result<Self> {
    let context = Self::new(data_directory);
    context.os.validate_security()?;
    Ok(context)
  }

  #[cfg(gtk)]
  /// Create an explicitly non-persistent WebKitGTK context. Incognito
  /// WebViews may share this context without falling back to a durable store.
  pub fn new_ephemeral() -> crate::Result<Self> {
    Ok(Self {
      os: WebContextImpl::new_ephemeral()?,
      data_directory: None,
      custom_protocols: Default::default(),
    })
  }

  /// Show the context's web processes `path` inside their sandbox. WebKit
  /// accepts this only before the context's first web process, and aborts
  /// after it: call it right after creating the context, before any WebView.
  #[cfg(gtk)]
  pub fn add_path_to_sandbox(&self, path: &Path, read_only: bool) {
    use crate::webkitgtk::WebContextExt as _;
    use webkit2gtk::WebContextExt as _;
    self.context().add_path_to_sandbox(path, read_only);
  }

  /// A reference to the data directory the context was created with.
  pub fn data_directory(&self) -> Option<&Path> {
    self.data_directory.as_deref()
  }

  #[cfg(any(
    target_os = "linux",
    target_os = "dragonfly",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd",
  ))]
  pub(crate) fn register_custom_protocol(&mut self, name: String) -> Result<(), crate::Error> {
    if self.is_custom_protocol_registered(&name) {
      return Err(crate::Error::ContextDuplicateCustomProtocol(name));
    }
    self.custom_protocols.insert(name);
    Ok(())
  }

  /// Check if a custom protocol has been registered on this context.
  pub fn is_custom_protocol_registered(&self, name: &str) -> bool {
    self.custom_protocols.contains(name)
  }

  /// Set if this context allows automation.
  ///
  /// **Note:** This is currently only enforced on Linux, and has the stipulation that
  /// only 1 context allows automation at a time.
  pub fn set_allows_automation(&mut self, flag: bool) {
    self.os.set_allows_automation(flag);
  }
}

impl Default for WebContext {
  /// Creates a pathless context. On Linux/BSD this is ephemeral; persistent
  /// contexts must use [`WebContext::new`] with an explicit data directory.
  fn default() -> Self {
    Self::new(None)
  }
}

#[cfg(all(test, gtk))]
mod tests {
  use super::*;
  use crate::webkitgtk::WebContextExt as _;
  use crate::{
    NavigationEventPhase, WebViewBuilder, WebViewBuilderExtUnix as _, WebViewExtUnix as _,
  };
  use gtk::prelude::*;
  use webkit2gtk::{WebContextExt as _, WebViewExt as _};

  #[derive(Default)]
  struct PresentationObservations {
    stage_mapping_authorized: std::cell::Cell<bool>,
    presentation_authorized: std::cell::Cell<bool>,
    child_observed: std::cell::Cell<bool>,
    guarded_at_parenting: std::cell::Cell<bool>,
    unauthorized_surface: std::cell::Cell<bool>,
    map_count: std::cell::Cell<u32>,
    revealed_draw_count: std::cell::Cell<u32>,
  }

  fn pump_native_events_until(
    description: &str,
    timeout: std::time::Duration,
    mut complete: impl FnMut() -> bool,
  ) {
    let deadline = std::time::Instant::now() + timeout;
    loop {
      while gtk::events_pending() {
        gtk::main_iteration_do(false);
      }
      if complete() {
        return;
      }
      assert!(
        std::time::Instant::now() < deadline,
        "timed out waiting for {description}"
      );
      std::thread::sleep(std::time::Duration::from_millis(5));
    }
  }

  #[test]
  #[ignore = "requires a native GTK display"]
  fn tauri_style_incognito_and_gtk_reparent_invariants_are_native_and_fail_closed() {
    gtk::init().expect("GTK display");
    let mut context = WebContext::try_new(None).expect("verified pathless context");
    assert!(context.context().is_ephemeral());
    let window = gtk::Window::new(gtk::WindowType::Toplevel);
    let fixed = gtk::Fixed::new();
    window.add(&fixed);
    let view = WebViewBuilder::new_with_web_context(&mut context)
      .with_incognito(true)
      .build_gtk(&fixed)
      .expect("Tauri-style supplied incognito context");
    assert!(view.webview().is_ephemeral());
    assert_eq!(
      view
        .webview()
        .context()
        .as_ref()
        .map(|value| value.as_ptr()),
      Some(context.context().as_ptr())
    );
    drop(view);
    drop(window);

    let nonce = std::time::SystemTime::now()
      .duration_since(std::time::UNIX_EPOCH)
      .expect("system clock")
      .as_nanos();
    let directory = std::env::temp_dir().join(format!(
      "wry-persistent-incognito-rejection-{}-{nonce}",
      std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&directory);
    let mut persistent =
      WebContext::try_new(Some(directory.clone())).expect("verified persistent context");
    assert!(!persistent.context().is_ephemeral());
    let detached = gtk::Fixed::new();
    assert!(matches!(
      WebViewBuilder::new_with_web_context(&mut persistent)
        .with_incognito(true)
        .build_gtk(&detached),
      Err(crate::Error::NonEphemeralIncognitoContext)
    ));
    drop(persistent);
    let _ = std::fs::remove_dir_all(directory);

    crate::webkitgtk::assert_native_gtk_reparent_invariants();
  }

  #[test]
  #[ignore = "requires a native GTK display"]
  fn guarded_webkitgtk_construction_commit_and_first_map_are_native_and_fail_closed() {
    gtk::init().expect("GTK display");
    let window = gtk::Window::new(gtk::WindowType::Toplevel);
    window.set_default_size(640, 480);
    let fixed = gtk::Fixed::new();
    window.add(&fixed);
    window.show_all();
    pump_native_events_until(
      "mapped GTK test host",
      std::time::Duration::from_secs(5),
      || window.is_mapped(),
    );

    let observations = std::rc::Rc::new(PresentationObservations::default());
    let add_observations = observations.clone();
    fixed.connect_add(move |_, child| {
      add_observations.child_observed.set(true);
      add_observations.guarded_at_parenting.set(
        !child.is_visible()
          && !child.is_child_visible()
          && !child.is_mapped()
          && !child.is_sensitive()
          && !child.has_focus()
          && child.opacity() == 0.0,
      );

      let map_observations = add_observations.clone();
      child.connect_map(move |_| {
        map_observations
          .map_count
          .set(map_observations.map_count.get().saturating_add(1));
        if !map_observations.stage_mapping_authorized.get() {
          map_observations.unauthorized_surface.set(true);
        }
      });

      let sensitivity_observations = add_observations.clone();
      child.connect_notify_local(Some("sensitive"), move |widget, _| {
        if widget.is_sensitive() && !sensitivity_observations.presentation_authorized.get() {
          sensitivity_observations.unauthorized_surface.set(true);
        }
      });

      let focus_observations = add_observations.clone();
      child.connect_notify_local(Some("has-focus"), move |widget, _| {
        if widget.has_focus() && !focus_observations.presentation_authorized.get() {
          focus_observations.unauthorized_surface.set(true);
        }
      });

      let draw_observations = add_observations.clone();
      child.connect_draw(move |_, _| {
        if draw_observations.presentation_authorized.get() {
          draw_observations.revealed_draw_count.set(
            draw_observations
              .revealed_draw_count
              .get()
              .saturating_add(1),
          );
        }
        gtk::glib::Propagation::Proceed
      });
    });

    let guard_calls = std::rc::Rc::new(std::cell::Cell::new(0_u32));
    let guard_calls_callback = guard_calls.clone();
    let phases = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    let phases_callback = phases.clone();
    let mut context = WebContext::new_ephemeral().expect("secure ephemeral WebContext");
    let view = WebViewBuilder::new_with_web_context(&mut context)
      .with_bounds(crate::Rect {
        position: dpi::LogicalPosition::new(80, 60).into(),
        size: dpi::LogicalSize::new(320, 240).into(),
      })
      .with_visible(true)
      .with_focused(true)
      .with_navigation_presentation_guard(move || {
        guard_calls_callback.set(guard_calls_callback.get().saturating_add(1));
      })
      .with_navigation_event_handler(move |event| {
        phases_callback.borrow_mut().push(event.phase);
      })
      .build_gtk(&fixed)
      .expect("guarded WebKitGTK view");
    let widget = view.webview();

    assert!(observations.child_observed.get());
    assert!(observations.guarded_at_parenting.get());
    assert_eq!(observations.map_count.get(), 0);
    assert!(!observations.unauthorized_surface.get());
    assert!(!widget.is_child_visible());
    assert!(!widget.is_mapped());
    assert!(!widget.is_sensitive());
    assert!(!widget.has_focus());
    assert_eq!(widget.opacity(), 0.0);

    // The regression that motivated this gate was an ancestor `show_all`
    // remapping guarded raw views over trusted chrome. Ordinary visibility may
    // become true, but GTK's child-mapping barrier and input denial must hold.
    window.show_all();
    let ancestor_settled_at = std::time::Instant::now() + std::time::Duration::from_millis(250);
    pump_native_events_until(
      "ancestor show_all settlement",
      std::time::Duration::from_secs(1),
      || std::time::Instant::now() >= ancestor_settled_at,
    );
    assert!(!observations.unauthorized_surface.get());
    assert_eq!(observations.map_count.get(), 0);
    assert!(!widget.is_child_visible());
    assert!(!widget.is_mapped());
    assert!(!widget.is_sensitive());
    assert!(!widget.has_focus());

    view
      .load_html(
        "<!doctype html><html><head><title>wry-guarded-first-map</title>\
         <style>html,body{width:100%;height:100%;margin:0;background:rgb(37,173,37)}</style>\
         </head><body></body></html>",
      )
      .expect("local guarded navigation");
    pump_native_events_until(
      "guarded WebKitGTK commit and completion",
      std::time::Duration::from_secs(10),
      || {
        assert!(!observations.unauthorized_surface.get());
        let phases = phases.borrow();
        phases.contains(&NavigationEventPhase::Committed)
          && phases.contains(&NavigationEventPhase::Finished)
          && view.document_title().ok().flatten().as_deref() == Some("wry-guarded-first-map")
      },
    );
    assert!(guard_calls.get() > 0);
    assert_eq!(observations.map_count.get(), 0);
    assert!(!widget.is_child_visible());
    assert!(!widget.is_mapped());
    assert!(!widget.is_sensitive());
    assert!(!widget.has_focus());
    assert_eq!(widget.opacity(), 0.0);

    // Mirror Zephium Stage's pending path: establish the WebKit compositor at
    // a correctly-sized allocation wholly left of the trusted composition
    // root, while paint and input remain revoked.
    observations.stage_mapping_authorized.set(true);
    fixed.move_(&widget, -321, 0);
    widget.set_size_request(320, 240);
    widget.set_child_visible(true);
    widget.show_all();
    widget.queue_resize();
    widget.queue_draw();
    pump_native_events_until(
      "guarded offscreen first map",
      std::time::Duration::from_secs(5),
      || widget.is_mapped(),
    );
    let parked_x: i32 = fixed.child_property(&widget, "x");
    assert_eq!(parked_x, -321);
    assert!(widget.is_child_visible());
    assert!(widget.is_mapped());
    assert!(!widget.is_sensitive());
    assert!(!widget.has_focus());
    assert_eq!(widget.opacity(), 0.0);
    assert!(!observations.unauthorized_surface.get());

    // Simulate exact chrome acknowledgement: geometry and paint settle first;
    // input is restored last. A post-reveal snapshot must contain the local
    // page's distinctive green surface, proving that an initially guarded
    // view can establish and later present a real rendered backing store.
    observations.presentation_authorized.set(true);
    widget.set_size_request(320, 240);
    fixed.move_(&widget, 80, 60);
    widget.set_opacity(1.0);
    widget.queue_resize();
    widget.queue_draw();
    widget.set_sensitive(true);
    pump_native_events_until(
      "revealed WebKitGTK draw",
      std::time::Duration::from_secs(5),
      || observations.revealed_draw_count.get() > 0,
    );
    assert!(widget.is_mapped());
    assert!(widget.is_sensitive());
    assert_eq!(widget.opacity(), 1.0);
    assert!(!observations.unauthorized_surface.get());

    let snapshot_result = std::rc::Rc::new(std::cell::Cell::new(None));
    let snapshot_callback = snapshot_result.clone();
    widget.snapshot(
      webkit2gtk::SnapshotRegion::Visible,
      webkit2gtk::SnapshotOptions::NONE,
      None::<&gtk::gio::Cancellable>,
      move |result| {
        let rendered = result
          .ok()
          .and_then(|surface| gtk::cairo::ImageSurface::try_from(surface).ok())
          .is_some_and(|mut surface| {
            let (width, height, stride) = (surface.width(), surface.height(), surface.stride());
            if width <= 0 || height <= 0 || stride <= 0 {
              return false;
            }
            let offset = (height as usize / 2)
              .saturating_mul(stride as usize)
              .saturating_add((width as usize / 2).saturating_mul(4));
            surface.data().ok().is_some_and(|pixels| {
              let Some(pixel) = pixels.get(offset..offset.saturating_add(4)) else {
                return false;
              };
              // Cairo ARGB32 is native-endian. In either byte order, the
              // opaque test color (37, 173, 37) has two low components, one
              // middle component, and one high alpha component.
              pixel.iter().filter(|component| **component < 90).count() >= 2
                && pixel
                  .iter()
                  .any(|component| (120..=220).contains(component))
                && pixel.iter().any(|component| *component > 230)
            })
          });
        snapshot_callback.set(Some(rendered));
      },
    );
    pump_native_events_until(
      "rendered WebKitGTK snapshot",
      std::time::Duration::from_secs(5),
      || snapshot_result.get().is_some(),
    );
    assert_eq!(snapshot_result.get(), Some(true));

    drop(view);
    window.close();
    while gtk::events_pending() {
      gtk::main_iteration_do(false);
    }
  }
}

#[cfg(not(gtk))]
#[derive(Debug)]
pub(crate) struct WebContextImpl;

#[cfg(not(gtk))]
impl WebContextImpl {
  fn new(_: Option<&Path>) -> Self {
    Self
  }

  fn set_allows_automation(&mut self, _flag: bool) {}
}
