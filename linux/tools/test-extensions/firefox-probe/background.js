// Firefox style: the listener answers by returning a promise.
browser.runtime.onMessage.addListener(async (message) => {
  await browser.storage.local.set({ seen: Date.now() });
  return { firefox: Boolean(message.hello) };
});

// A context menu item, a keyboard command and a cookie round trip: each
// marks the page's title through the content script.
const mark = (tab, text) => tab && browser.tabs.sendMessage(tab.id, { mark: text });
browser.menus.create({ id: "probe-menu", title: "Firefox Probe: mark page", contexts: ["page", "link"] });
browser.menus.onClicked.addListener((info, tab) => mark(tab, `menu ${info.menuItemId}${info.linkUrl ? " link" : ""}`));
browser.commands.onCommand.addListener(async (command, tab) => {
  tab = tab || (await browser.tabs.query({ active: true, currentWindow: true }))[0];
  const url = tab.url;
  await browser.cookies.set({ url, name: "zephium_probe", value: "1" });
  const found = await browser.cookies.get({ url, name: "zephium_probe" });
  mark(tab, `cmd ${command} cookie ${found ? found.value : "none"}`);
});
