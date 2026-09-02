import { expect, test } from "@playwright/test";
import { seedMockOverrides, seedOnboarded } from "./helpers";

test("v0.7 updater migration is visible and preserves both data paths", async ({
  page,
}) => {
  await seedOnboarded(page);
  await seedMockOverrides(page, {
    load_data_migration_notice: {
      legacyDataDir: "/home/community/.arc",
      activeDataDir: "/home/community/.arc/data-v3",
      migratedAt: 1_787_878_787_000,
      reason: "pre-v3 WAL had no authenticated genesis.network-hash binding",
    },
  });

  await page.goto("/");
  const banner = page.getByTestId("data-migration-banner");
  await expect(banner).toBeVisible();
  await expect(banner).toContainText("protected your old block history");
  await expect(page.getByTestId("legacy-data-dir")).toHaveText(
    "/home/community/.arc",
  );
  await expect(page.getByTestId("active-v3-data-dir")).toHaveText(
    "/home/community/.arc/data-v3",
  );
  await expect(banner).toContainText("identity and model selection");
  await expect(page.getByTestId("data-migration-reason")).toContainText(
    "no authenticated genesis.network-hash binding",
  );

  await page.getByTestId("dismiss-data-migration").click();
  await expect(banner).toHaveCount(0);
});

test("malformed genesis binding is surfaced as an explicit recoverable fence", async ({
  page,
}) => {
  await seedOnboarded(page);
  await seedMockOverrides(page, {
    load_data_migration_notice: {
      legacyDataDir: "/home/community/.arc/ambiguous-v3",
      activeDataDir: "/home/community/.arc/ambiguous-v3/data-v3",
      migratedAt: 1_787_878_787_000,
      reason:
        "malformed genesis.network-hash made the existing chain directory ambiguous; original bytes were preserved and never replayed",
    },
  });

  await page.goto("/");
  const banner = page.getByTestId("data-migration-banner");
  await expect(banner).toContainText("could not be safely replayed");
  await expect(page.getByTestId("data-migration-reason")).toContainText(
    "malformed genesis.network-hash",
  );
  await expect(banner).toContainText("binding, and block byte untouched");
});
