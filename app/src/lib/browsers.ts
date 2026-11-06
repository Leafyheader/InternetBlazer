import type { Browser, ExtensionInstalled } from "./api";

/** What the user does next after the extension was put into a browser. */
export function installMessage(browser: Browser, installed: ExtensionInstalled): string {
  return installed === "onRestart"
    ? `${browser.name}: restart it, then turn on InternetBlazer when it asks.`
    : `${browser.name}: its store page opened. Click Add there.`;
}
