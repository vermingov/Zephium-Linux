browser.runtime.sendMessage({ hello: 1 }).then(
  (reply) => { document.title = `[ff ${reply && reply.firefox ? "ok" : "bad"}] ${document.title}`; },
  (error) => { document.title = `[ff err ${error.message}] ${document.title}`; },
);
