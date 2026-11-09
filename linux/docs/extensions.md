# Browser extensions on Linux

Upstream Zephium runs Chrome extensions on macOS through WebKit's own extension
engine (WKWebExtension) and on Windows through WebView2. WebKitGTK has no such
engine: WebKit's port of it to the GLib platforms is unreleased, GTK4-only and
behind an experimental build flag (see "Why not WebKit's engine" below). The
Linux port therefore brings its own extension runtime, built on public
WebKitGTK API and borrowing what Firefox and Chromium publish:

- Firefox's WebExtension API schemas (MPL-2.0, the same license as Zephium)
  describe every API, its arguments and its events.
- Chromium's extension API definitions (BSD) fill in Chrome-only details.
- Mozilla's webextension-polyfill gives extensions written for `browser.*`
  the promise API on top of the `chrome.*` one.

GNOME Web has its own runtime on WebKitGTK too. It is GPL-3.0 and is not
copied here.

Steps, each tested in `tools/rig` before the next:

1. A messaging channel between extension code and the browser (this page).
2. Upstream's install, review, registry, update and toolbar code switched on
   for Linux; it is platform-neutral and only gated to macOS and Windows.
3. The runtime: `chrome-extension://` pages, background pages (Manifest V3
   service workers run in one), content scripts, popups, and the common APIs.

Then declarativeNetRequest through WebKit's content filters, and Firefox
packages (`.xpi`, `moz-extension://`, addons.mozilla.org).

## 1. The messaging channel

### Why upstream keeps its channel out of product builds

`platform/linux/mod.rs` already has a per-extension channel, compiled only into
tests: each extension gets an isolated script world and a script message
handler registered for that world alone. Upstream leaves it out of products
for two reasons, both stated beside the code:

1. WebKit serializes a message in the web process and rebuilds it as a
   JavaScriptCore value in the browser process before any Rust runs. A 64 KiB
   limit checked in Rust comes after an attacker-sized allocation in the
   trusted process.
2. Extension code in that world can call the raw handler,
   `window.webkit.messageHandlers.<name>.postMessage`, directly, so nothing a
   JavaScript wrapper checks can be relied on.

Re-enabling that channel would reopen both. The Linux port does not.

### The port's channel: a web-process plugin

WebKitGTK loads web-process plugins from the directory given to
`webkit_web_context_set_web_extensions_directory`, which WebKit also adds to
the content-process sandbox read-only. The port ships one plugin,
`libzephium-webext-process.so`, and no script message handler for extensions.

- **The plugin owns the extension worlds.** For each enabled extension it
  creates one isolated world (`webkit_script_world_new_with_name`). A world the
  browser creates for user scripts is a different world even under the same
  name, because each creation gets a fresh content-world identifier, and only
  worlds the plugin created get `window-object-cleared`. So the plugin also
  injects the extension's content scripts, which the browser sends it.
- **One native function per world.** On `window-object-cleared` the plugin
  defines a JavaScriptCore function in that world's global object. It is the
  only way out of the world. It accepts a string and a reply callback, rejects
  anything else, and measures the string inside the web process. Only a
  message within the limit is turned into a `WebKitUserMessage` and sent with
  `webkit_web_page_send_message_to_view`. The browser process never receives
  more than the limit, so there is no trusted-side allocation to bound.
  Measuring in the web process costs at most a copy of a string the sender
  already allocated in that same, sandboxed process.
- **Identity comes from the plugin, never from JavaScript.** The plugin tags
  each message with the extension of the world it created. Page scripts run in
  the main world and cannot reach an isolated world's globals; one extension's
  world cannot reach another's.
- **Extension pages** (background, popup, options) run in the main world of
  their own views at `chrome-extension://<id>/`. The plugin gives the same
  function to main-world frames whose document URL has that scheme, with the
  extension ID taken from the URL host, which WebKit sets and page code cannot.
- **Replies and events.** The browser answers with
  `webkit_user_message_send_reply`; the plugin calls the stored callback in
  the sending world. Events (`runtime.onMessage`, `tabs.onUpdated`, ...) go
  from the browser with `webkit_web_view_send_message_to_page` to the
  plugin, which calls the dispatcher the world registered.
- **The browser checks again.** It accepts plugin messages only for
  extensions that are enabled and loaded in that view, and it re-applies the
  limit, as defence in depth.

Nothing is loosened: the sandbox, the WebKitGTK version floor and
process-per-site stay as upstream sets them, and the plugin does nothing in
worlds and frames that belong to no extension.

## Why not WebKit's engine

As of WebKit main on 2026-11-07: `WebKitWebExtensionContext` is marked 2.56,
`WebKitWebExtensionManager` (the controller) is an open pull request, most
APIs (tabs, windows, action, scripting, declarativeNetRequest) are still
Cocoa-only, the GLib extension URL-scheme handler is unimplemented, and the
whole feature is compiled only into webkitgtk-6.0 (GTK4) with
`ENABLE_WK_WEB_EXTENSIONS`, which distributions leave off. wry and Zephium use
webkit2gtk-4.1. When WebKit's engine ships on a platform the port can use, the
runtime here can step aside for it.

## 2. Upstream's extension code on Linux

Upstream's install, review, registry, update and toolbar code is
platform-neutral; it is compiled only for macOS and Windows. The port widens
those gates to Linux and adds `Platform::Linux` to the limits list. Packages
are prepared as on macOS (`zephium_webext::prepare`): the compatibility
script goes first in every extension page (`<script>`), at the top of the
service worker (`importScripts`), and first in the manifest's background and
content-script lists. On Linux that script is the runtime's API layer.

## 3. The runtime

`crates/zephium-webext-linux` holds the JavaScript layer and the browser's
side of every API; `zephium-engine`'s `host/webext_linux.rs` connects it to
WebKitGTK and the engine's extension contract (`Engine::load_web_extension`
and the rest, as on macOS).

- **Pages.** A profile's context serves `chrome-extension://<id>/...` (and
  `moz-extension://`, the same files) from the prepared package, with the
  manifest's CSP. A request from a page that is not the extension's own gets
  only `web_accessible_resources`.
- **Background.** One hidden view per enabled extension loads a generated
  page: the manifest's `background.page`, or its `background.scripts`, or its
  `service_worker` as a classic or module script after the API layer. Host
  permissions become the view's CORS allowlist.
- **Content scripts.** The plugin's startup data carries each extension's
  `content_scripts` groups (matches, exclude_matches, js, css, run_at,
  all_frames); the plugin runs them in the extension's world, `document_end`
  and `document_idle` deferred to the matching document events.
- **Popups and options.** The toolbar action opens a popover holding a view
  of the popup page; options open in a tab.

### Protocol between the API layer and the browser

Over the channel's one function, as JSON:

- request `{"id": n, "api": "storage.local.get", "args": [...]}`, reply
  `{"id": n, "ok": value}` or `{"id": n, "error": "message"}`, which the API
  layer turns into a resolved promise, a callback, or `runtime.lastError`;
- event, sent by the browser to a world: `{"event": "runtime.onMessage",
  "args": [...], "reply": token}`; a listener's answer goes back as request
  `runtime.answer` with that token.

The plugin gets a second function, `__zephiumWebextListen(listener)`, which
registers the world's one event listener; the browser reaches it with the
`zephium-webext.event` message, addressed to a page and frame.

### API coverage, first round

runtime (messaging and ports, getURL, getManifest, id, lastError, onInstalled,
onStartup), storage (local, sync, session, onChanged; kept per profile),
tabs, windows, scripting, action / browserAction / pageAction (badge, icon,
title, popup), i18n, alarms, permissions, contextMenus, commands,
notifications, cookies, webNavigation, extension. declarativeNetRequest
follows through WebKit's content filters. `browser.*` is Mozilla's
webextension-polyfill over `chrome.*`.

## 4. Firefox add-ons

An `.xpi` is a zip, installed like an unpacked Chrome package. A Gecko ID
(`browser_specific_settings.gecko.id`, or the file's name) maps to a stable
Chrome-style ID, the SHA-256 of the Gecko ID spelled in the letters a-p, so
upstream's registry and origins keep working; `moz-extension://` serves the
same package. The install button appears on addons.mozilla.org listings as
upstream's does on the Chrome Web Store.

## 5. Blocking webRequest

Content blockers written for Firefox (uBlock Origin) decide each request in
a `webRequest.onBeforeRequest` listener with `["blocking"]`. WebKitGTK's one
hook into a page's requests is `WebKitWebPage::send-request`, raised in the
web process and answered on the spot: returning true cancels the request, a
new URI redirects it.

- **The plugin asks, synchronously.** The listeners run in the extension's
  background, another process. WebKit's own messages cannot carry the
  answer: their replies arrive on the web process's main loop, which the
  wait blocks. So the browser serves a Unix socket for this alone
  (`platform/linux/request_broker.rs`), in a directory of its own under the
  runtime directory, mode 0700. Its path goes in the plugin's configuration;
  `webkit_web_context_add_path_to_sandbox` shows the content processes that
  directory, read-only, which is enough to connect. Nothing else in the
  sandbox changes.
- **Only what someone listens for.** The configuration carries the URL
  patterns of every background's `onBeforeRequest` listeners; other
  requests, and every request of an extension's own pages, go out unasked.
- **One line each way.** Page ID, frame number, resource type, method, URL
  and the page's address; the answer is `allow`, `cancel` or `redirect` and
  a URL. The resource type comes from `Sec-Fetch-Dest`, which WebKit sets;
  without it, `Accept` and the file name guess.
- **The browser** finds the tab of the page, raises the event in each
  background whose listener's filter and host permissions match, and waits
  for the blocking ones (`host/webext_linux/web_request.rs`): a cancel wins,
  then the first redirect to the web, a data URL or the extension's own
  package. It answers `allow` after 2 s; the plugin stops waiting at 3 s.
- **Not covered:** the later events (`onBeforeSendHeaders`,
  `onHeadersReceived`, `onCompleted`, ...) exist but never fire, so header
  rewriting and HTML filtering do not happen. WebKit does not say which
  frame a request comes from: what a page loads counts as its main frame's.
  WebSocket connections are not offered.
