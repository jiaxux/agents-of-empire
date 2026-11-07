import { expect, test as base } from "@playwright/test";
import { spawnAoeServe } from "../helpers/aoeServe";

base("Fleet Operations stays lazy and exposes the shared live projection", async ({ page }, testInfo) => {
  const serve = await spawnAoeServe({
    authMode: "none",
    workerIndex: testInfo.workerIndex,
    parallelIndex: testInfo.parallelIndex,
  });

  try {
    let fleetRequests = 0;
    page.on("request", (request) => {
      if (new URL(request.url()).pathname === "/api/fleet") fleetRequests += 1;
    });

    await page.goto(`${serve.baseUrl}/`);
    const open = page.getByRole("button", { name: "Fleet Operations" });
    await expect(open).toBeVisible({ timeout: 10_000 });
    expect(fleetRequests).toBe(0);

    const responsePromise = page.waitForResponse(
      (response) => new URL(response.url()).pathname === "/api/fleet" && response.request().method() === "GET",
    );
    await open.click();
    const response = await responsePromise;
    expect(response.status()).toBe(200);
    const report = (await response.json()) as { schema_version: number; sessions: number };
    expect(report.schema_version).toBe(1);
    expect(report.sessions).toBe(0);
    expect(fleetRequests).toBe(1);

    const dialog = page.getByRole("dialog", { name: "Fleet Operations" });
    await expect(dialog).toBeVisible();
    await page.keyboard.press("ArrowRight");
    await expect(page.getByRole("tab", { name: /conflicts/i })).toHaveAttribute("aria-selected", "true");

    await page.keyboard.press("/");
    const filter = page.getByRole("textbox", { name: "Filter Fleet Operations" });
    await expect(filter).toBeFocused();
    await filter.fill("missing");
    await page.keyboard.press("Escape");
    await expect(filter).toHaveValue("");
    await page.keyboard.press("Escape");
    await expect(dialog).toBeHidden();
  } finally {
    await serve.stop();
  }
});
