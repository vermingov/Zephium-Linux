// Answers the content script, remembers where it was, and shows a badge.
chrome.runtime.onMessage.addListener((message, sender, respond) => {
  if (message.type !== "ping") return false;
  chrome.storage.local.set({ lastPing: sender.tab ? sender.tab.url : "no tab" });
  chrome.action.setBadgeText({ text: "OK" });
  respond({ pong: true, tab: sender.tab ? sender.tab.id : null });
  return false;
});
