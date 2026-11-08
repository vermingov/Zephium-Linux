// Firefox style: the listener answers by returning a promise.
browser.runtime.onMessage.addListener(async (message) => {
  await browser.storage.local.set({ seen: Date.now() });
  return { firefox: Boolean(message.hello) };
});
