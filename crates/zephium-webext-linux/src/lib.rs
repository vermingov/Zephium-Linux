//! The Linux port's extension runtime.
//!
//! WebKitGTK has no extension engine, so the port brings its own: [`SCRIPT`],
//! the API layer every extension context loads first, and the parts of the
//! browser's side that do not depend on WebKit (what each context is told at
//! start, the content scripts the web-process plugin runs, and storage). The
//! design and the protocol are in the Linux port's docs/extensions.md.

pub mod content;
pub mod dnr;
pub mod info;
pub mod storage;

/// The API layer: `chrome` and `browser` over the web-process plugin's
/// channel. `zephium_webext::prepare` puts it first in every extension page,
/// background and content-script list.
pub const SCRIPT: &str = include_str!("../js/runtime.js");
