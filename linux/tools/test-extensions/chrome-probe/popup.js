chrome.storage.local.get("lastPing").then((stored) => {
  document.getElementById("out").textContent = `last ping from: ${stored.lastPing || "nothing yet"}`;
});
