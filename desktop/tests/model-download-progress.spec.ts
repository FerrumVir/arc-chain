import { expect, test } from "@playwright/test";
import { modelDownloadStatus } from "../src/lib/format";
import type { ModelDownloadProgress } from "../src/lib/types";

// Pure logic: no page. The copy the onboarding screen and the observer
// upgrade dialog show for each stage of a resumable model download.
const GIB = 1024 ** 3;
const base: ModelDownloadProgress = {
  tier: "standard",
  downloadedBytes: GIB,
  totalBytes: 4_081_004_224,
  done: false,
};

test.describe("model download status copy", () => {
  test("reports bytes and percent while downloading", () => {
    expect(modelDownloadStatus({ ...base, stage: "downloading" })).toBe(
      "1.00 GB of 3.80 GB (26%)",
    );
    // Events from a backend that predates stages read the same way.
    expect(modelDownloadStatus(base)).toBe("1.00 GB of 3.80 GB (26%)");
  });

  test("says a resumed download continues instead of restarting", () => {
    expect(
      modelDownloadStatus({ ...base, stage: "connecting", resumedFromBytes: GIB }),
    ).toBe("Resuming from 1.00 GB of 3.80 GB…");
    expect(
      modelDownloadStatus({ ...base, stage: "downloading", resumedFromBytes: GIB }),
    ).toBe("1.00 GB of 3.80 GB (26%) · resumed");
    expect(
      modelDownloadStatus({ ...base, stage: "connecting", downloadedBytes: 0 }),
    ).toBe("Connecting to the model mirror…");
  });

  test("explains a retry and says the saved part is kept", () => {
    expect(
      modelDownloadStatus({
        ...base,
        stage: "retrying",
        retryInSecs: 8,
        message: "the connection dropped while downloading",
      }),
    ).toBe(
      "Connection interrupted at 1.00 GB of 3.80 GB (26%) (the connection dropped while downloading). Retrying in 8 s; the downloaded part is kept.",
    );
  });

  test("names verification and completion", () => {
    expect(
      modelDownloadStatus({ ...base, stage: "verifying", downloadedBytes: base.totalBytes }),
    ).toBe("Verifying all 3.80 GB against the pinned SHA-256…");
    expect(modelDownloadStatus({ ...base, stage: "done", done: true })).toBe(
      "Downloaded and verified 3.80 GB.",
    );
  });
});
