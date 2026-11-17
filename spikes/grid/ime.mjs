// Serves the page for checking input methods by hand.
//
//   node spikes/grid/ime.mjs [--browser firefox|chromium]
//
// Input through an input method cannot be automated faithfully, so the check
// is done by a person: open the address this prints in the browser to be
// checked and follow the list on the page. With --browser the script opens
// that browser itself, in the build Playwright downloads, and stays until its
// window is closed; that is the way to check Firefox on a machine that has no
// Firefox installed. Stop the script with Ctrl+C.
//
// `cargo xtask spike s10` builds the page first and then runs this.

import { chromium, firefox } from "@playwright/test";
import { serve } from "./serve.mjs";

const browsers = { chromium, firefox };
const [flag, name] = process.argv.slice(2);
if (flag !== undefined && (flag !== "--browser" || !(name in browsers))) {
  console.error("usage: node spikes/grid/ime.mjs [--browser firefox|chromium]");
  process.exit(2);
}

// A fixed port, so that the address stays the same from one run to the next.
const server = await serve(5275);
const address = `${server.address}/ime.html`;
console.log(`The page is at ${address}`);

if (name) {
  const browser = await browsers[name].launch({ headless: false });
  // No fixed viewport: the page follows the window, and the browser's own
  // zoom works as it does for a user.
  const context = await browser.newContext({ viewport: null });
  const page = await context.newPage();
  await page.goto(address);
  console.log(`Opened ${name} ${browser.version()}. Close its window when done.`);
  await new Promise((closed) => browser.on("disconnected", closed));
  server.close();
} else {
  console.log("Open it in the browser to be checked. Ctrl+C stops this.");
}
