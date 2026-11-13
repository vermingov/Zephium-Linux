browser.runtime.sendMessage({ hello: 1 }).then(
  (reply) => { document.title = `[ff ${reply && reply.firefox ? "ok" : "bad"}] ${document.title}`; },
  (error) => { document.title = `[ff err ${error.message}] ${document.title}`; },
);
browser.runtime.onMessage.addListener((message) => {
  if (message.mark) document.title = `[${message.mark}] ${document.title}`;
});
