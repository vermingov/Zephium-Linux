//! Browser extensions on Linux: the engine's side of the port's runtime.
//!
//! WebKitGTK has no extension engine, so a profile's extensions run on the
//! port's own pieces: the web-process plugin (`zephium-webext-process`) runs
//! content scripts and carries messages, `zephium-webext-linux` holds the API
//! layer and storage, and this module loads packages, serves their pages, keeps
//! one hidden background view per extension, answers API calls and draws the
//! toolbar actions. Design: the Linux port's docs/extensions.md.

mod action;
mod api;
mod routing;
mod scheme;

/// Log one step of the runtime when `ZEPHIUM_WEBEXT_TRACE` is set; silent
/// otherwise. For finding where an extension stops working.
macro_rules! trace {
    ($($argument:tt)*) => {
        if std::env::var_os("ZEPHIUM_WEBEXT_TRACE").is_some() {
            eprintln!("webext: {}", format!($($argument)*));
        }
    };
}
pub(super) use trace;

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::rc::Rc;

use webkit2gtk::{WebContext, WebView, WebViewExt};
use wry::WebViewExtUnix;
use zephium_core::extensions::ExtensionRuntimeGeneration;
use zephium_core::ids::{ExtensionInstallId, ProfileId};
use zephium_core::ports::engine::{EngineEvent, WebExtensionLoad, WebExtensionLoaded};
use zephium_webext::manifest::{Background, Manifest};

use crate::platform::imp::extension_channel::{self, Channel, ChannelExtension};

/// Every profile's extensions.
#[derive(Default)]
pub(super) struct LinuxExtensions {
    profiles: HashMap<ProfileId, ProfileExtensions>,
    /// Extension IDs whose pages a profile's tabs may open, shared with each
    /// view's event permit as on Windows.
    navigation: HashMap<ProfileId, super::permits::ExtensionNavigationGrants>,
    /// Numbers the tab and window requests this runtime raises.
    next_request: u64,
}

impl LinuxExtensions {
    pub(super) fn navigation_grants(&mut self, profile: ProfileId) -> super::permits::ExtensionNavigationGrants {
        self.navigation.entry(profile).or_default().clone()
    }

    fn set_navigation(&mut self, profile: ProfileId, ids: HashSet<String>) {
        let grants = self.navigation_grants(profile);
        *grants.lock().unwrap_or_else(|error| error.into_inner()) = ids;
    }
}

/// One profile's extensions and what they run in.
#[derive(Default)]
pub(super) struct ProfileExtensions {
    /// The profile's WebKit context, once one of its views exists.
    context: Option<WebContext>,
    channel: Option<Rc<Channel>>,
    packages: scheme::Packages,
    installs: HashMap<ExtensionInstallId, Install>,
    routing: routing::Routing,
}

impl ProfileExtensions {
    fn install(&self, extension_id: &str) -> Option<&Install> {
        self.installs.values().find(|install| install.extension_id == extension_id)
    }

    fn install_mut(&mut self, extension_id: &str) -> Option<&mut Install> {
        self.installs.values_mut().find(|install| install.extension_id == extension_id)
    }
}

/// One loaded extension.
pub(super) struct Install {
    pub(super) id: ExtensionInstallId,
    pub(super) extension_id: String,
    pub(super) generation: ExtensionRuntimeGeneration,
    pub(super) manifest: Manifest,
    pub(super) root: PathBuf,
    pub(super) loaded: WebExtensionLoaded,
    pub(super) channel: ChannelExtension,
    pub(super) package: scheme::Package,
    pub(super) permissions: Vec<String>,
    pub(super) hosts: Vec<String>,
    pub(super) background: Option<WebView>,
    pub(super) storage: api::Storage,
    pub(super) action: action::ActionState,
    pub(super) alarms: HashMap<String, gtk::glib::SourceId>,
    pub(super) popup: Option<action::Popup>,
    /// Tabs the user clicked the extension's action on: `activeTab` lets it
    /// script them although it has no host permission for them.
    pub(super) active_tabs: HashSet<zephium_core::ids::ItemId>,
}

impl Install {
    pub(super) fn origin(&self) -> String {
        format!("chrome-extension://{}/", self.extension_id)
    }

    /// The extension's own views that are not tabs: its background and its
    /// open popup.
    pub(super) fn own_views(&self) -> Vec<WebView> {
        self.background.iter().cloned().chain(self.popup.as_ref().map(|popup| popup.view.clone())).collect()
    }

    fn prepare(
        load: &WebExtensionLoad,
        generation: ExtensionRuntimeGeneration,
        storage: Option<PathBuf>,
    ) -> Result<Self, String> {
        let manifest = Manifest::load(&load.root).map_err(|error| error.to_string())?;
        let origin = format!("chrome-extension://{}/", load.extension_id);
        let info = zephium_webext_linux::info::info(&load.extension_id, &origin, &load.root, &manifest);
        let groups = zephium_webext_linux::content::groups(&load.root, &manifest)?
            .into_iter()
            .map(|group| (group.matches, group.exclude_matches, group.js, group.run_at.to_owned(), group.all_frames))
            .collect();
        let loaded = WebExtensionLoaded {
            name: manifest.name().unwrap_or_else(|| load.extension_id.clone()),
            version: manifest.version().unwrap_or("0").to_owned(),
        };
        let package = scheme::Package {
            root: load.root.clone(),
            csp: content_security_policy(&manifest),
            accessible: web_accessible(&manifest),
            background: background_page(&manifest),
        };
        Ok(Self {
            id: load.install,
            extension_id: load.extension_id.clone(),
            generation,
            root: load.root.clone(),
            loaded,
            channel: ChannelExtension {
                id: load.extension_id.clone(),
                info,
                granted: load.match_patterns.clone(),
                groups,
            },
            package,
            permissions: load.permissions.clone(),
            hosts: load.match_patterns.clone(),
            background: None,
            storage: api::Storage::open(storage)?,
            action: action::ActionState::from_manifest(&manifest, &load.root),
            alarms: HashMap::new(),
            popup: None,
            active_tabs: HashSet::new(),
            manifest,
        })
    }

    fn stop(&mut self) {
        for (_, alarm) in self.alarms.drain() {
            alarm.remove();
        }
        if let Some(popup) = self.popup.take() {
            popup.close();
        }
        if let Some(background) = self.background.take() {
            background.try_close();
        }
    }
}

impl super::EngineHost {
    pub(crate) fn load_web_extension(&mut self, profile: ProfileId, load: WebExtensionLoad) {
        let previous = self.linux_stop_install(profile, load.install).map(|install| install.generation);
        let generation = previous
            .and_then(|generation| generation.next())
            .unwrap_or(ExtensionRuntimeGeneration::INITIAL);
        let storage = self.linux_storage_dir(profile, &load.extension_id);
        let result = Install::prepare(&load, generation, storage).map(|install| {
            let loaded = install.loaded.clone();
            let state = self.linux_extensions.profiles.entry(profile).or_default();
            state.installs.insert(load.install, install);
            loaded
        });
        self.linux_extensions_sync(profile);
        self.sink.emit(EngineEvent::WebExtensionSettled { profile, install: load.install, result });
        self.sink.emit(EngineEvent::ExtensionActionsInvalidated { profile });
    }

    pub(crate) fn unload_web_extension(&mut self, profile: ProfileId, install: ExtensionInstallId) {
        self.linux_stop_install(profile, install);
        self.linux_extensions_sync(profile);
        self.sink.emit(EngineEvent::ExtensionActionsInvalidated { profile });
    }

    pub(crate) fn remove_web_extension(&mut self, profile: ProfileId, load: WebExtensionLoad) {
        self.linux_stop_install(profile, load.install);
        if let Some(storage) = self.linux_storage_dir(profile, &load.extension_id) {
            let _ = std::fs::remove_dir_all(storage);
        }
        self.linux_extensions_sync(profile);
        self.sink.emit(EngineEvent::ExtensionActionsInvalidated { profile });
    }

    pub(crate) fn open_web_extension_options(&mut self, profile: ProfileId, extension_id: &str) {
        let Some(state) = self.linux_extensions.profiles.get(&profile) else {
            return;
        };
        let Some(install) = state.install(extension_id) else {
            return;
        };
        let Some(page) = install.manifest.options_page() else {
            return;
        };
        let url = format!("{}{}", install.origin(), page.trim_start_matches('/'));
        self.linux_request_tab(profile, Some(url), true);
    }

    /// A view of `profile` passed its security configuration: the profile's
    /// context now serves extension pages and carries the plugin.
    pub(super) fn linux_extensions_view_ready(&mut self, profile: ProfileId, view: &wry::WebView) {
        let Some(context) = view.webview().context() else {
            return;
        };
        let state = self.linux_extensions.profiles.entry(profile).or_default();
        if state.context.is_none() {
            scheme::register(&context, &state.packages);
            if let Some(channel) = extension_channel::channel(&context) {
                channel.set_handler(move |message| {
                    super::dispatch::best_effort_with(move |host| host.linux_extension_message(profile, message));
                });
                state.channel = Some(channel);
            }
            state.context = Some(context);
        }
        self.linux_extensions_sync(profile);
    }

    /// Bring the plugin's startup data, the served packages, the background
    /// views and the navigation grants in line with the loaded installs.
    fn linux_extensions_sync(&mut self, profile: ProfileId) {
        let tab_views = self.linux_tab_views(profile);
        let Some(state) = self.linux_extensions.profiles.get_mut(&profile) else {
            return;
        };
        {
            let mut packages = state.packages.borrow_mut();
            packages.clear();
            for install in state.installs.values() {
                packages.insert(install.extension_id.clone(), install.package.clone());
            }
        }
        if let Some(channel) = &state.channel {
            channel.set_extensions(state.installs.values().map(|install| install.channel.clone()).collect());
            // Processes already running learn the change now; new ones ask.
            let data = channel.current_data();
            let own = state.installs.values().flat_map(Install::own_views);
            for view in tab_views.iter().cloned().chain(own) {
                extension_channel::configure_page(&view, &data);
            }
        }
        if let (Some(context), Some(channel)) = (&state.context, &state.channel) {
            for install in state.installs.values_mut().filter(|install| install.background.is_none()) {
                install.background = start_background(context, channel, install);
            }
        }
        let ids = state.installs.values().map(|install| install.extension_id.clone()).collect();
        self.linux_extensions.set_navigation(profile, ids);
    }

    fn linux_stop_install(&mut self, profile: ProfileId, install: ExtensionInstallId) -> Option<Install> {
        let state = self.linux_extensions.profiles.get_mut(&profile)?;
        let mut removed = state.installs.remove(&install)?;
        removed.stop();
        state.routing.forget_extension(&removed.extension_id);
        Some(removed)
    }

    /// Where an extension's `local` and `sync` storage live in its profile.
    fn linux_storage_dir(&self, profile: ProfileId, extension_id: &str) -> Option<PathBuf> {
        crate::erasure::prepare_profile_directory(&self.profiles_root, profile)
            .ok()
            .map(|profile_dir| profile_dir.join("webext-storage").join(extension_id))
    }
}

/// The hidden view an extension's background runs in. Host permissions are
/// its CORS allowlist, as in Chrome.
fn start_background(context: &WebContext, channel: &Rc<Channel>, install: &Install) -> Option<WebView> {
    let path = match install.manifest.background()? {
        Background::Page(page) => page,
        Background::Scripts(_) | Background::ServiceWorker { .. } => scheme::BACKGROUND_PATH.trim_start_matches('/').to_owned(),
    };
    let view = WebView::with_context(context);
    let allowlist = cors_allowlist(&install.hosts);
    view.set_cors_allowlist(&allowlist.iter().map(String::as_str).collect::<Vec<_>>());
    extension_channel::listen(&view, channel.clone());
    let url = format!("{}{}", install.origin(), path.trim_start_matches('/'));
    trace!("background of {} starts at {url}", install.extension_id);
    view.connect_load_changed(|view, event| trace!("background {:?}: {event:?}", view.uri()));
    view.connect_load_failed(|view, _, uri, error| {
        trace!("background {:?} failed to load {uri}: {error}", view.uri());
        false
    });
    view.connect_web_process_terminated(|view, reason| trace!("background {:?}: web process ended ({reason:?})", view.uri()));
    view.load_uri(&url);
    Some(view)
}

/// Chrome host patterns in the form WebKit's allowlist takes.
fn cors_allowlist(hosts: &[String]) -> Vec<String> {
    hosts
        .iter()
        .flat_map(|host| match host.as_str() {
            "<all_urls>" => vec!["*://*/*".to_owned()],
            other => vec![other.to_owned()],
        })
        .collect()
}

/// The generated page a background without its own page loads. Preparation
/// put the API layer first in `background.scripts`; a service worker gets it,
/// and a stand-in for `importScripts`, ahead of the worker's script.
fn background_page(manifest: &Manifest) -> Option<String> {
    let compat = format!("/{}/compat.js", zephium_webext::prepare::COMPAT_DIR);
    let tag = |src: &str, module: bool| {
        let kind = if module { r#" type="module""# } else { "" };
        format!(r#"<script src="/{}"{kind}></script>"#, src.trim_start_matches('/'))
    };
    let scripts = match manifest.background()? {
        Background::Page(_) => return None,
        Background::Scripts(scripts) => scripts.iter().map(|script| tag(script, false)).collect::<Vec<_>>(),
        Background::ServiceWorker { path, module } => {
            vec![tag(&compat, false), tag(scheme::WORKER_SHIM_PATH, false), tag(&path, module)]
        }
    };
    Some(format!("<!doctype html><meta charset=\"utf-8\">{}", scripts.concat()))
}

fn content_security_policy(manifest: &Manifest) -> String {
    let raw = &manifest.raw()["content_security_policy"];
    let declared = raw["extension_pages"].as_str().or_else(|| raw.as_str());
    declared.map(str::to_owned).unwrap_or_else(|| {
        if manifest.manifest_version() >= 3 {
            "script-src 'self' 'wasm-unsafe-eval'; object-src 'self';".to_owned()
        } else {
            "script-src 'self' blob: filesystem:; object-src 'self';".to_owned()
        }
    })
}

/// `web_accessible_resources` in either manifest version's shape.
fn web_accessible(manifest: &Manifest) -> Vec<String> {
    let Some(entries) = manifest.web_accessible_resources().and_then(|value| value.as_array()) else {
        return Vec::new();
    };
    entries
        .iter()
        .flat_map(|entry| match entry {
            serde_json::Value::String(path) => vec![path.clone()],
            object => object["resources"]
                .as_array()
                .map(|resources| resources.iter().filter_map(|value| value.as_str().map(str::to_owned)).collect())
                .unwrap_or_default(),
        })
        .collect()
}
