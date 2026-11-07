chrome.storage.local.get(["lastPing", "events"]).then((stored) => {
  const events = Object.entries(stored.events || {}).map(([name, count]) => `${name} ${count}`).join(", ");
  document.getElementById("out").textContent = `last ping from: ${stored.lastPing || "nothing yet"}`;
  document.getElementById("events").textContent = `events: ${events || "none yet"}`;
});
