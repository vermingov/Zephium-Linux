// Answers the content script, remembers where it was, and shows a badge.
chrome.runtime.onMessage.addListener((message, sender, respond) => {
  if (message.type !== "ping") return false;
  chrome.storage.local.set({ lastPing: sender.tab ? sender.tab.url : "no tab" });
  chrome.action.setBadgeText({ text: "OK" });
  respond({ pong: true, tab: sender.tab ? sender.tab.id : null });
  return false;
});

// Counts tab and window events; the popup shows the tally.
const seen = {};
for (const [area, names] of [["tabs", ["onCreated", "onUpdated", "onActivated", "onRemoved"]], ["windows", ["onFocusChanged"]]]) {
  for (const name of names) {
    chrome[area][name].addListener(() => {
      const key = `${area}.${name}`;
      seen[key] = (seen[key] || 0) + 1;
      chrome.storage.local.set({ events: seen });
    });
  }
}
