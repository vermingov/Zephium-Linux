// Zephium builds on macOS, Windows and Linux; stop elsewhere before Cargo.
if (!["darwin", "win32", "linux"].includes(process.platform)) {
  console.error(`Zephium isn't available on ${process.platform}.`);
  process.exit(1);
}
