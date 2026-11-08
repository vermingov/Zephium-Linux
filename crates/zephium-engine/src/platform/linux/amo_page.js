// On an add-on's page at addons.mozilla.org, the store offers other browsers
// "Download Firefox and get the extension". Zephium installs Firefox add-ons
// itself, so that button becomes "Add to Zephium" and opens the file the
// page's own "Download file" link names; the browser fetches it and shows its
// install review. Runs in a script world of its own; only trusted clicks
// count. Requalify when the store changes its markup.
(() => {
  if (window !== top || location.protocol !== 'https:' || location.hostname !== 'addons.mozilla.org') return;
  const LABEL = 'Add to Zephium';
  const FILE = /^https:\/\/addons\.mozilla\.org\/(?:[^?#]*\/)?downloads\/file\/\d+\/[^/?#]+\.xpi$/;
  const taken = new WeakSet();

  const fileLink = () =>
    [...document.querySelectorAll('a[href*="/downloads/file/"]')].find((link) => FILE.test(link.href)) ?? null;

  // The store's main button sits beside the file link.
  const storeButton = (link) => {
    for (let box = link.parentElement; box && box !== document.body; box = box.parentElement) {
      const button = [...box.querySelectorAll('a, button')].find(
        (element) => element !== link && /Button/.test(element.className),
      );
      if (button) return button;
    }
    return null;
  };

  const click = (event) => {
    const link = fileLink();
    if (!event.isTrusted || !link) return;
    event.preventDefault();
    event.stopImmediatePropagation();
    location.assign(link.href);
  };

  const update = () => {
    const link = fileLink();
    const button = link && storeButton(link);
    if (!button) return;
    if (button.textContent !== LABEL) button.textContent = LABEL;
    if (!taken.has(button)) {
      taken.add(button);
      button.addEventListener('click', click, true);
    }
  };

  new MutationObserver(update).observe(document, { childList: true, subtree: true, characterData: true });
  update();
})();
