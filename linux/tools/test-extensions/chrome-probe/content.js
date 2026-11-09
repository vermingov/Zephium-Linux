// Pings the background; the page title shows whether the answer came back.
chrome.runtime.sendMessage({ type: "ping" }, (reply) => {
  const error = chrome.runtime.lastError ? chrome.runtime.lastError.message : "";
  const verdict = reply && reply.pong ? "pong" : `fail ${error}`;
  document.documentElement.dataset.zephiumProbe = verdict;
  document.title = `[probe ${verdict}] ${document.title}`;
});
