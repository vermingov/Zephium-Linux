// Zephium's extension API on Linux.
//
// Loaded first in every extension context: extension pages and the background
// page get it as their first <script>, content scripts get it first in their
// isolated world. It builds `chrome` (and `browser`, the same object, since
// every method returns a promise when called without a callback) on top of the
// web-process plugin's two functions:
//
//   __zephiumWebextSend(json, reply)   one request to the browser
//   __zephiumWebextListen(listener)    the world's one event listener
//
// and the plugin's read-only `__zephiumWebextInfo` (JSON: the extension's ID,
// base URL, manifest and resolved messages) and `__zephiumWebextContext`
// ("page" or "content"). The protocol is in the Linux port's docs/extensions.md.
(() => {
  "use strict";

  const send = globalThis.__zephiumWebextSend;
  const listen = globalThis.__zephiumWebextListen;
  if (typeof send !== "function" || typeof listen !== "function") return;
  if (globalThis.chrome && globalThis.chrome.runtime && globalThis.chrome.runtime.id) return;

  const info = JSON.parse(globalThis.__zephiumWebextInfo);
  const isContentScript = globalThis.__zephiumWebextContext === "content";

  // ---- Requests -------------------------------------------------------------

  let nextRequest = 1;

  function request(api, args) {
    return new Promise((resolve, reject) => {
      const id = nextRequest++;
      send(JSON.stringify({ id, api, args }), (answer) => {
        if (answer === undefined) {
          reject(new Error("The browser did not answer."));
          return;
        }
        const reply = JSON.parse(answer);
        if ("error" in reply) reject(new Error(reply.error));
        else resolve(reply.ok);
      });
    });
  }

  // chrome.runtime.lastError is set only while a callback that got an error
  // runs, as in Chrome; promise callers get the rejection instead.
  let lastError;

  function withLastError(error, callback) {
    lastError = { message: error.message };
    try {
      callback();
    } finally {
      lastError = undefined;
    }
  }

  // A method that takes an optional trailing callback and otherwise returns a
  // promise. `shape` turns the browser's answer into what callers get.
  function method(api, shape = (value) => value) {
    return (...args) => {
      const callback = typeof args[args.length - 1] === "function" ? args.pop() : null;
      const pending = request(api, args).then(shape);
      if (!callback) return pending;
      pending.then(
        (value) => callback(value),
        (error) => withLastError(error, () => callback()),
      );
      return undefined;
    };
  }

  // Menu items' onclick callbacks, by item ID.
  const menuClicks = new Map();

  // A promise delivered as a method's answer would be.
  function settle(pending, callback) {
    if (!callback) return pending;
    pending.then((value) => callback(value), (error) => withLastError(error, () => callback()));
    return undefined;
  }

  // downloads.download: the file is read here, where its blob or data URL
  // lives, and sent to the browser in pieces small enough for the channel.
  async function saveDownload(options) {
    const response = await fetch(options.url);
    if (!response.ok) throw new Error(`The download failed: ${response.status}`);
    const bytes = new Uint8Array(await response.arrayBuffer());
    const name = options.filename || new URL(options.url).pathname.split("/").pop() || "download";
    const token = await request("downloads.begin", [name]);
    const piece = 36 * 1024; // 48 KiB once in base64, inside the channel's limit
    for (let at = 0; at < bytes.length; at += piece) {
      let text = "";
      for (const byte of bytes.subarray(at, at + piece)) text += String.fromCharCode(byte);
      await request("downloads.chunk", [token, btoa(text)]);
    }
    return request("downloads.finish", [token]);
  }

  // An answer known in this context, delivered as a method's would be.
  function answer(value) {
    return (...args) => {
      const callback = typeof args[args.length - 1] === "function" ? args.pop() : null;
      if (!callback) return Promise.resolve(value);
      queueMicrotask(() => callback(value));
      return undefined;
    };
  }

  // ---- Events ---------------------------------------------------------------

  class ExtensionEvent {
    constructor() {
      this.listeners = [];
    }
    addListener(listener) {
      if (!this.hasListener(listener)) this.listeners.push(listener);
    }
    removeListener(listener) {
      this.listeners = this.listeners.filter((known) => known !== listener);
    }
    hasListener(listener) {
      return this.listeners.includes(listener);
    }
    hasListeners() {
      return this.listeners.length > 0;
    }
    dispatch(args) {
      return this.listeners.slice().map((listener) => {
        try {
          return listener(...args);
        } catch (error) {
          console.error(error);
          return undefined;
        }
      });
    }
  }

  const events = new Map();

  // ---- webRequest -------------------------------------------------------------

  // Chrome match patterns as regular expressions, for listener filters.
  function patternRegExp(pattern) {
    if (pattern === "<all_urls>") return /^(?:https?|wss?|file|ftp):\/\//;
    const match = /^(\*|[a-z][a-z0-9+.-]*):\/\/(\*|\*\.[^/*]+|[^/*]*)(\/.*)$/.exec(pattern);
    if (!match) return /$^/;
    const [, scheme, host, path] = match;
    const escape = (text) => text.replace(/[.+?^${}()|[\]\\]/g, "\\$&");
    const schemePart = scheme === "*" ? "(?:https?|wss?)" : escape(scheme);
    const hostPart = host === "*" ? "[^/]*" : host.startsWith("*.") ? `(?:[^/]*\\.)?${escape(host.slice(2))}(?::\\d+)?` : `${escape(host)}(?::\\d+)?`;
    const pathPart = path.split("*").map(escape).join(".*");
    return new RegExp(`^${schemePart}:\\/\\/(?:[^/@]*@)?${hostPart}${pathPart}$`);
  }

  // One webRequest event. Listeners come with a filter and extra options;
  // the browser learns every listener's filter, so it asks only about
  // requests someone wants. A blocking listener's answer (or the promise of
  // one) cancels or redirects the request: a cancel wins, then the first
  // redirect.
  class WebRequestEvent {
    constructor(name) {
      this.name = name;
      this.entries = [];
    }
    addListener(listener, filter, extraInfoSpec) {
      if (typeof listener !== "function" || this.hasListener(listener)) return;
      const urls = Array.isArray(filter?.urls) ? filter.urls.map(String) : ["<all_urls>"];
      this.entries.push({
        listener,
        urls,
        matchers: urls.map(patternRegExp),
        types: Array.isArray(filter?.types) ? filter.types.map(String) : null,
        tabId: typeof filter?.tabId === "number" ? filter.tabId : null,
        blocking: Array.isArray(extraInfoSpec) && extraInfoSpec.includes("blocking"),
      });
      reportWebRequestListeners();
    }
    removeListener(listener) {
      const before = this.entries.length;
      this.entries = this.entries.filter((entry) => entry.listener !== listener);
      if (this.entries.length !== before) reportWebRequestListeners();
    }
    hasListener(listener) {
      return this.entries.some((entry) => entry.listener === listener);
    }
    hasListeners() {
      return this.entries.length > 0;
    }
    async deliver(details) {
      const response = {};
      for (const entry of this.entries.slice()) {
        if (entry.types && !entry.types.includes(details.type)) continue;
        if (entry.tabId !== null && entry.tabId !== details.tabId) continue;
        if (!entry.matchers.some((matcher) => matcher.test(details.url))) continue;
        let result;
        try {
          result = await entry.listener({ ...details });
        } catch (error) {
          console.error(error);
          continue;
        }
        if (!entry.blocking || !result || typeof result !== "object") continue;
        if (result.cancel === true) return { cancel: true };
        if (typeof result.redirectUrl === "string" && response.redirectUrl === undefined) {
          response.redirectUrl = result.redirectUrl;
        }
      }
      return response;
    }
  }

  const webRequestEvents = new Map(
    ["onBeforeRequest", "onBeforeSendHeaders", "onSendHeaders", "onHeadersReceived", "onAuthRequired",
      "onResponseStarted", "onBeforeRedirect", "onCompleted", "onErrorOccurred"]
      .map((name) => [`webRequest.${name}`, new WebRequestEvent(name)]),
  );

  // The browser gets the whole list after each change, once per turn.
  let reportQueued = false;
  function reportWebRequestListeners() {
    if (reportQueued) return;
    reportQueued = true;
    queueMicrotask(() => {
      reportQueued = false;
      const listeners = [];
      for (const target of webRequestEvents.values()) {
        for (const { urls, types, blocking } of target.entries) {
          listeners.push({ event: target.name, urls, types, blocking });
        }
      }
      request("webRequest.listeners", [listeners]).catch(() => {});
    });
  }

  function event(name) {
    if (!events.has(name)) events.set(name, new ExtensionEvent());
    return events.get(name);
  }

  listen((raw) => {
    const message = JSON.parse(raw);
    if (message.event === "runtime.onMessage" || message.event === "runtime.onMessageExternal") {
      deliverMessage(event(message.event), message.args, message.reply);
    } else if (message.event === "port.message") {
      const port = ports.get(message.args[0]);
      if (port) port.onMessage.dispatch([message.args[1], port]);
    } else if (message.event === "port.disconnect") {
      const port = ports.get(message.args[0]);
      if (port) closePort(port, message.args[1]);
    } else if (message.event === "runtime.onConnect") {
      const [id, name, sender] = message.args;
      event("runtime.onConnect").dispatch([openPort(id, name, sender)]);
    } else if (webRequestEvents.has(message.event)) {
      const decided = webRequestEvents.get(message.event).deliver(message.args[0]);
      if (message.reply !== undefined) {
        decided.then((response) => request("webRequest.answer", [message.reply, response]));
      }
    } else if (events.has(message.event)) {
      events.get(message.event).dispatch(message.args || []);
    }
  });

  // ---- One-off messages -----------------------------------------------------

  // A listener answers with sendResponse, by returning true and calling it
  // later, or (the `browser.*` way) by returning a promise. The first answer
  // wins; when no listener will answer, the sender learns at once.
  function deliverMessage(target, [message, sender], token) {
    let answered = false;
    const answer = (value) => {
      if (answered) return;
      answered = true;
      request("runtime.answer", [token, value === undefined ? null : value, true]);
    };
    let waiting = false;
    for (const result of target.dispatch([message, sender, answer])) {
      if (result === true) waiting = true;
      else if (result && typeof result.then === "function") {
        waiting = true;
        result.then(answer, (error) => {
          if (!answered) {
            answered = true;
            request("runtime.answer", [token, String(error && error.message), false]);
          }
        });
      }
    }
    if (!waiting && !answered) {
      answered = true;
      request("runtime.answer", [token, null, false]);
    }
  }

  // ---- Ports ----------------------------------------------------------------

  const ports = new Map();

  function openPort(id, name, sender) {
    const port = {
      name,
      sender,
      onMessage: new ExtensionEvent(),
      onDisconnect: new ExtensionEvent(),
      postMessage(message) {
        if (!ports.has(id)) throw new Error("Attempting to use a disconnected port object");
        request("port.post", [id, message]);
      },
      disconnect() {
        if (ports.delete(id)) request("port.disconnect", [id]);
      },
    };
    ports.set(id, port);
    return port;
  }

  function closePort(port, error) {
    for (const [id, known] of ports) if (known === port) ports.delete(id);
    if (error) withLastError(new Error(error), () => port.onDisconnect.dispatch([port]));
    else port.onDisconnect.dispatch([port]);
  }

  // connect() must hand back a port at once, before the browser has numbered
  // it, so messages posted early wait for the number.
  function connector(api) {
    return (...args) => {
      const pending = [];
      let id = null;
      const port = {
        name: "",
        onMessage: new ExtensionEvent(),
        onDisconnect: new ExtensionEvent(),
        postMessage(message) {
          if (id === null) pending.push(message);
          else request("port.post", [id, message]);
        },
        disconnect() {
          if (id !== null && ports.delete(id)) request("port.disconnect", [id]);
          else pending.length = 0;
        },
      };
      const options = args.find((arg) => arg && typeof arg === "object") || {};
      port.name = options.name || "";
      request(api, args).then(
        (number) => {
          id = number;
          ports.set(id, port);
          for (const message of pending.splice(0)) request("port.post", [id, message]);
        },
        (error) => closePort(port, error.message),
      );
      return port;
    };
  }

  // ---- The API surface ------------------------------------------------------

  // namespace: [methods (name -> browser API name, or a function), events]
  function namespace(name, methods, eventNames = []) {
    const target = {};
    for (const [key, value] of Object.entries(methods)) {
      target[key] = typeof value === "function" ? value : method(value || `${name}.${key}`);
    }
    for (const eventName of eventNames) {
      Object.defineProperty(target, eventName, { get: () => event(`${name}.${eventName}`), enumerable: true });
    }
    return target;
  }

  function storageArea(area) {
    const target = namespace(`storage.${area}`, {
      get: `storage.get`,
      set: `storage.set`,
      remove: `storage.remove`,
      clear: `storage.clear`,
      getBytesInUse: `storage.getBytesInUse`,
    }, ["onChanged"]);
    // Every storage method names its area first.
    for (const key of ["get", "set", "remove", "clear", "getBytesInUse"]) {
      const call = target[key];
      target[key] = (...args) => call(area, ...args);
    }
    return target;
  }

  const runtime = namespace("runtime", {
    sendMessage: "runtime.sendMessage",
    connect: connector("runtime.connect"),
    getPlatformInfo: undefined,
    openOptionsPage: undefined,
    setUninstallURL: undefined,
    reload: undefined,
    getURL: (path) => new URL(String(path), info.baseUrl).href,
    getManifest: () => JSON.parse(JSON.stringify(info.manifest)),
    // Firefox's: add-ons ask which browser runs them.
    getBrowserInfo: () => Promise.resolve({ name: "Zephium", vendor: "Zephium", version: "1.0", buildID: "" }),
  }, ["onMessage", "onMessageExternal", "onConnect", "onInstalled", "onStartup", "onSuspend", "onUpdateAvailable"]);
  Object.defineProperty(runtime, "id", { value: info.id, enumerable: true });
  Object.defineProperty(runtime, "lastError", { get: () => lastError, enumerable: true });

  const i18n = {
    getMessage(name, substitutions) {
      const entry = info.messages[String(name).toLowerCase()];
      if (!entry) return "";
      const values = substitutions === undefined ? [] : [].concat(substitutions).map(String);
      return entry.replace(/\$(\d)/g, (_, index) => values[Number(index) - 1] ?? "");
    },
    getUILanguage: () => info.uiLanguage,
    detectLanguage: answer({ isReliable: false, languages: [] }),
    getAcceptLanguages: method("i18n.getAcceptLanguages"),
  };

  const extension = {
    getURL: runtime.getURL,
    inIncognitoContext: false,
    isAllowedIncognitoAccess: method("extension.isAllowedIncognitoAccess"),
    // The port gives extensions no access to file:// pages.
    isAllowedFileSchemeAccess: answer(false),
    // Each context runs in a process of its own: no other context's window
    // can be handed over.
    getViews: () => [],
    getBackgroundPage: () => null,
  };

  const storage = {
    local: storageArea("local"),
    sync: storageArea("sync"),
    session: storageArea("session"),
    // Administrators set nothing for extensions here: always empty.
    managed: {
      get: answer({}),
      getBytesInUse: answer(0),
      onChanged: event("storage.managed.onChanged"),
    },
    onChanged: event("storage.onChanged"),
  };

  const chrome = { runtime, i18n, extension, storage };

  if (!isContentScript) {
    const tabs = namespace("tabs", {
      query: undefined, get: undefined, getCurrent: undefined, create: undefined, update: undefined,
      remove: undefined, reload: undefined, duplicate: undefined, move: undefined, sendMessage: undefined,
      executeScript: undefined, insertCSS: undefined, removeCSS: undefined,
      connect: connector("tabs.connect"), captureVisibleTab: undefined, captureTab: undefined, detectLanguage: undefined,
    }, ["onCreated", "onUpdated", "onActivated", "onRemoved", "onReplaced", "onMoved", "onHighlighted"]);
    tabs.TAB_ID_NONE = -1;

    const windows = namespace("windows", {
      get: undefined, getCurrent: undefined, getLastFocused: undefined, getAll: undefined,
      create: undefined, update: undefined, remove: undefined,
    }, ["onCreated", "onRemoved", "onFocusChanged"]);
    windows.WINDOW_ID_NONE = -1;
    windows.WINDOW_ID_CURRENT = -2;

    const action = namespace("action", {
      setTitle: undefined, getTitle: undefined, setIcon: undefined, setPopup: undefined,
      getPopup: undefined, setBadgeText: undefined, getBadgeText: undefined,
      setBadgeBackgroundColor: undefined, getBadgeBackgroundColor: undefined,
      setBadgeTextColor: undefined, getBadgeTextColor: undefined,
      enable: undefined, disable: undefined, isEnabled: undefined, openPopup: undefined,
      getUserSettings: undefined,
    }, ["onClicked"]);

    Object.assign(chrome, {
      tabs,
      windows,
      action,
      // MV2 names for the same toolbar action.
      browserAction: action,
      pageAction: action,
      scripting: namespace("scripting", {
        // A function cannot travel as JSON: it goes as the code that calls it.
        executeScript: (injection, callback) => {
          const { func, args, ...rest } = injection || {};
          const code = typeof func === "function" ? `(${func})(...${JSON.stringify(args || [])})` : undefined;
          return method("scripting.executeScript")({ ...rest, code }, callback);
        },
        insertCSS: undefined, removeCSS: undefined,
        registerContentScripts: undefined, getRegisteredContentScripts: undefined,
        unregisterContentScripts: undefined, updateContentScripts: undefined,
      }),
      declarativeNetRequest: Object.assign(namespace("declarativeNetRequest", {
        getEnabledRulesets: undefined, updateEnabledRulesets: undefined,
        getDynamicRules: undefined, updateDynamicRules: undefined,
        getSessionRules: undefined, updateSessionRules: undefined,
        isRegexSupported: undefined, getAvailableStaticRuleCount: undefined,
        getDisabledRuleIds: undefined, updateStaticRules: undefined,
        getMatchedRules: undefined, setExtensionActionOptions: undefined,
      }, ["onRuleMatchedDebug"]), {
        MAX_NUMBER_OF_ENABLED_STATIC_RULESETS: 50, MAX_NUMBER_OF_STATIC_RULESETS: 100,
        MAX_NUMBER_OF_DYNAMIC_RULES: 30000, MAX_NUMBER_OF_SESSION_RULES: 5000,
        MAX_NUMBER_OF_REGEX_RULES: 1000, GUARANTEED_MINIMUM_STATIC_RULES: 30000,
        DYNAMIC_RULESET_ID: "_dynamic", SESSION_RULESET_ID: "_session",
      }),
      alarms: namespace("alarms", {
        create: undefined, get: undefined, getAll: undefined, clear: undefined, clearAll: undefined,
      }, ["onAlarm"]),
      permissions: namespace("permissions", {
        contains: undefined, getAll: undefined, request: undefined, remove: undefined,
      }, ["onAdded", "onRemoved"]),
      contextMenus: namespace("contextMenus", {
        create: (properties, callback) => {
          const id = properties.id ?? `zephium-menu-${nextRequest}`;
          if (typeof properties.onclick === "function") menuClicks.set(id, properties.onclick);
          method("contextMenus.create")({ ...properties, id, onclick: undefined }, callback);
          return id;
        },
        update: (id, properties, callback) => {
          if (typeof properties?.onclick === "function") menuClicks.set(id, properties.onclick);
          return method("contextMenus.update")(id, { ...properties, onclick: undefined }, callback);
        },
        remove: undefined, removeAll: undefined, refresh: undefined,
      }, ["onClicked", "onShown", "onHidden"]),
      commands: namespace("commands", { getAll: undefined }, ["onCommand"]),
      notifications: namespace("notifications", {
        create: undefined, update: undefined, clear: undefined, getAll: undefined,
      }, ["onClicked", "onClosed", "onButtonClicked"]),
      cookies: namespace("cookies", {
        get: undefined, getAll: undefined, set: undefined, remove: undefined, getAllCookieStores: undefined,
      }, ["onChanged"]),
      webNavigation: namespace("webNavigation", { getFrame: undefined, getAllFrames: undefined },
        ["onBeforeNavigate", "onCommitted", "onDOMContentLoaded", "onCompleted", "onErrorOccurred",
          "onHistoryStateUpdated", "onReferenceFragmentUpdated", "onCreatedNavigationTarget", "onTabReplaced"]),
    });
    // The browser's own stores, read and written by the shell; the argument
    // objects are the macOS port's, whose translation the browser shares.
    const data = (api, fields, callback) => settle(request(api, [fields]), callback);
    Object.assign(chrome, {
      history: namespace("history", {
        search: (details, callback) => data("history.search", { details }, callback),
        getVisits: (details, callback) => data("history.getVisits", { details }, callback),
        addUrl: (details, callback) => data("history.addUrl", { details }, callback),
        deleteUrl: (details, callback) => data("history.deleteUrl", { details }, callback),
        deleteRange: (details, callback) => data("history.deleteRange", { details }, callback),
        deleteAll: (callback) => data("history.deleteAll", { details: {} }, callback),
      }, ["onVisited", "onVisitRemoved", "onTitleChanged"]),
      bookmarks: namespace("bookmarks", {
        getTree: (callback) => data("bookmarks.getTree", {}, callback),
        getSubTree: (id, callback) => data("bookmarks.getSubTree", { id }, callback),
        getChildren: (id, callback) => data("bookmarks.getChildren", { id }, callback),
        get: (ids, callback) => data("bookmarks.get", { ids: [].concat(ids) }, callback),
        getRecent: (count, callback) => data("bookmarks.getRecent", { count }, callback),
        search: (query, callback) => data("bookmarks.search", { details: query }, callback),
        create: (bookmark, callback) => data("bookmarks.create", { details: bookmark || {} }, callback),
        update: (id, changes, callback) => data("bookmarks.update", { id, details: changes || {} }, callback),
        move: (id, destination, callback) => data("bookmarks.move", { id, details: destination || {} }, callback),
        remove: (id, callback) => data("bookmarks.remove", { id }, callback),
        removeTree: (id, callback) => data("bookmarks.removeTree", { id }, callback),
      }, ["onCreated", "onChanged", "onMoved", "onRemoved", "onChildrenReordered", "onImportBegan", "onImportEnded"]),
      sessions: namespace("sessions", {
        getRecentlyClosed: (filter, callback) => typeof filter === "function"
          ? data("sessions.getRecentlyClosed", {}, filter)
          : data("sessions.getRecentlyClosed", { filter }, callback),
        getDevices: (filter, callback) => settle(Promise.resolve([]), typeof filter === "function" ? filter : callback),
        restore: (sessionId, callback) => typeof sessionId === "function"
          ? data("sessions.restore", {}, sessionId)
          : data("sessions.restore", { sessionId }, callback),
      }, ["onChanged"]),
      topSites: namespace("topSites", { get: (callback) => data("topSites.get", {}, callback) }),
    });
    chrome.sessions.MAX_SESSION_RESULTS = 25;
    // onBeforeRequest sees every request a page makes, and may block it;
    // WebKitGTK gives the port no hook later in a request's life, so the
    // other events are present but never fire.
    const webRequest = { handlerBehaviorChanged: answer(undefined), MAX_HANDLER_BEHAVIOR_CHANGED_CALLS_PER_10_MINUTES: 20 };
    for (const [name, target] of webRequestEvents) {
      webRequest[name.slice("webRequest.".length)] = target;
    }
    // Firefox's enums; extensions read the types they may filter on here.
    webRequest.ResourceType = Object.fromEntries(
      ["main_frame", "sub_frame", "stylesheet", "script", "image", "font", "object", "xmlhttprequest", "ping",
        "beacon", "xslt", "csp_report", "media", "websocket", "web_manifest", "imageset", "other"]
        .map((type) => [type.toUpperCase(), type]),
    );
    webRequest.OnBeforeRequestOptions = { BLOCKING: "blocking", REQUEST_BODY: "requestBody", EXTRA_HEADERS: "extraHeaders" };
    webRequest.OnBeforeSendHeadersOptions = { REQUEST_HEADERS: "requestHeaders", BLOCKING: "blocking", EXTRA_HEADERS: "extraHeaders" };
    webRequest.OnHeadersReceivedOptions = { BLOCKING: "blocking", RESPONSE_HEADERS: "responseHeaders", EXTRA_HEADERS: "extraHeaders" };
    Object.assign(chrome, {
      webRequest,
      downloads: namespace("downloads", {
        download: (options, callback) => settle(saveDownload(options || {}), callback),
        search: undefined, erase: undefined, cancel: undefined,
      }, ["onCreated", "onChanged", "onErased", "onDeterminingFilename"]),
      idle: namespace("idle", { queryState: answer("active"), setDetectionInterval: answer(undefined) }, ["onStateChanged"]),
      management: namespace("management", {
        getSelf: answer({
          id: info.id, name: info.manifest.name, version: info.manifest.version, enabled: true,
          installType: "normal", type: "extension", mayDisable: true,
        }),
      }, ["onInstalled", "onUninstalled", "onEnabled", "onDisabled"]),
    });
    // Firefox's contentScripts.register, over scripting's
    // registrations: the promise gives an object that unregisters them. Code
    // goes along as text, kept by the browser until it is unregistered.
    chrome.contentScripts = {
      register: async (options) => {
        const files = (list) => (list || []).map((entry) => entry.file).filter(Boolean);
        const code = (list) => (list || []).map((entry) => entry.code).filter((text) => typeof text === "string");
        if (options.world === "MAIN") throw new Error("Content scripts in the page's own world are not available on Linux yet.");
        const id = `zephium-registered-${Date.now()}-${nextRequest}`;
        await request("scripting.registerContentScripts", [[{
          id, matches: options.matches, excludeMatches: options.excludeMatches,
          js: files(options.js), css: files(options.css), jsCode: code(options.js), cssCode: code(options.css),
          runAt: options.runAt, allFrames: options.allFrames, persistAcrossSessions: false,
        }]]);
        return { unregister: () => request("scripting.unregisterContentScripts", [{ ids: [id] }]) };
      },
    };
    // Firefox's name for contextMenus.
    chrome.menus = chrome.contextMenus;
    // An item's own onclick, which cannot travel to the browser, runs here.
    chrome.contextMenus.onClicked.addListener((info, tab) => menuClicks.get(info.menuItemId)?.(info, tab));
    Object.assign(runtime, {
      getBackgroundPage: () => Promise.reject(new Error("The background page runs in its own view on Linux.")),
    });
  }

  for (const name of ["chrome", "browser"]) {
    Object.defineProperty(globalThis, name, { value: chrome, configurable: false, writable: false });
  }
})();
