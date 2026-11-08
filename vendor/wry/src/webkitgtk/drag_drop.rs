// Copyright 2020-2023 Tauri Programme within The Commons Conservancy
// SPDX-License-Identifier: Apache-2.0
// SPDX-License-Identifier: MIT

use std::{
  cell::{Cell, RefCell},
  path::PathBuf,
  rc::Rc,
};

use gtk::{glib::GString, prelude::*};
use webkit2gtk::WebView;

use crate::DragDropEvent;

#[derive(PartialEq, Eq, PartialOrd, Ord, Clone, Copy, Debug)]
enum DragControllerState {
  Entered,
  Leaving,
  Left,
}

struct DragDropController {
  paths: RefCell<Option<Vec<PathBuf>>>,
  state: Cell<DragControllerState>,
  position: Cell<(i32, i32)>,
  handler: Box<dyn Fn(DragDropEvent) -> bool>,
}

impl DragDropController {
  fn new(handler: Box<dyn Fn(DragDropEvent) -> bool>) -> Self {
    Self {
      handler,
      paths: RefCell::new(None),
      state: Cell::new(DragControllerState::Left),
      position: Cell::new((0, 0)),
    }
  }

  fn store_paths(&self, paths: Vec<PathBuf>) -> bool {
    let Ok(mut stored_paths) = self.paths.try_borrow_mut() else {
      return false;
    };
    *stored_paths = Some(paths);
    true
  }

  fn take_paths(&self) -> Option<Vec<PathBuf>> {
    self.paths.try_borrow_mut().ok()?.take()
  }

  fn store_position(&self, position: (i32, i32)) {
    self.position.replace(position);
  }

  fn enter(&self) {
    self.state.set(DragControllerState::Entered);
  }

  fn leaving(&self) {
    self.state.set(DragControllerState::Leaving);
  }

  fn leave(&self) {
    self.state.set(DragControllerState::Left);
  }

  fn state(&self) -> DragControllerState {
    self.state.get()
  }

  fn call(&self, event: DragDropEvent) -> bool {
    (self.handler)(event)
  }
}

pub(crate) fn connect_drag_event(webview: &WebView, handler: Box<dyn Fn(DragDropEvent) -> bool>) {
  let controller = Rc::new(DragDropController::new(handler));

  {
    let controller = controller.clone();
    webview.connect_drag_data_received(move |_, _, _, _, data, info, _| {
      if info == 2 {
        let uris = data.uris();
        let paths = uris
          .iter()
          .filter_map(path_buf_from_uri)
          .collect::<Vec<_>>();
        if paths.is_empty() || !controller.store_paths(paths.clone()) {
          controller.leave();
          return;
        }
        controller.enter();
        controller.call(DragDropEvent::Enter {
          paths: paths.clone(),
          position: controller.position.get(),
        });
      }
    });
  }

  {
    let controller = controller.clone();
    webview.connect_drag_motion(move |_, _, x, y, _| {
      if controller.state() == DragControllerState::Entered {
        controller.call(DragDropEvent::Over { position: (x, y) });
      } else {
        controller.store_position((x, y));
      }
      false
    });
  }

  {
    let controller = controller.clone();
    webview.connect_drag_drop(move |_, ctx, x, y, time| {
      if controller.state() == DragControllerState::Leaving {
        if let Some(paths) = controller.take_paths() {
          controller.leave();
          let handled = controller.call(DragDropEvent::Drop {
            paths,
            position: (x, y),
          });
          // Finish the drop only when the embedder took it. Otherwise WebKit
          // still has to read the data: finished here, the page never got
          // the dropped files.
          if handled {
            ctx.drop_finish(true, time);
          }
          return handled;
        }
      }

      false
    });
  }

  webview.connect_drag_leave(move |_w, _, _| {
    if controller.state() != DragControllerState::Left {
      controller.leaving();
      let controller = controller.clone();
      gtk::glib::idle_add_local_once(move || {
        if controller.state() == DragControllerState::Leaving {
          controller.leave();
          controller.call(DragDropEvent::Leave);
        }
      });
    }
  });
}

fn path_buf_from_uri(gstr: &GString) -> Option<PathBuf> {
  // `gio::File::path` returns `None` for non-local URI schemes. Do not turn a
  // page URL or a remote file authority into a fabricated host filesystem path.
  gtk::gio::File::for_uri(gstr.as_str()).path()
}
