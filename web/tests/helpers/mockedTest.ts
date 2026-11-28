// Playwright `test` wrapper for the mocked suite under `web/tests/`.
//
// Re-exports `@playwright/test`'s `test` and `expect` with one override:
// the `page` fixture starts Chromium V8 coverage before the test and writes
// it after, so the merged-LCOV pipeline picks up coverage from the mocked
// specs the same way it does from live specs.
//
// Specs do:
//
//   import { test, expect } from "./helpers/mockedTest";
//
// `vite preview` serves the production bundle (built with inline sourcemaps
// when `AOE_COVERAGE=1`). Without that env var, coverage collection is a
// no-op and the override is invisible.

import { test as base, expect, type Page } from "@playwright/test";
import { startCoverage, stopAndWriteCoverage } from "./coverageCapture";

export const test = base.extend({
  page: async ({ page }, use, testInfo) => {
    const started = await startCoverage(page);
    await use(page);
    await stopAndWriteCoverage(page, testInfo.titlePath.join(" > "), started);
  },
});

/**
 * Press a global shortcut once its listener is attached; an early key is silently lost.
 * Does not click first: a click at the viewport centre can land on a pane button.
 */
export async function pressShortcut(page: Page, key: string) {
  await expect(page.locator("html[data-shortcuts-ready]")).toBeAttached();
  await page.keyboard.press(key);
}

export { expect };
