import { describe, expect, it } from "vitest";

import { normalizeThumbnailUrl, pickThumbnail } from "./index.js";

describe("normalizeThumbnailUrl", () => {
  it("promotes protocol-relative URLs to https", () => {
    // The exact shape youtubei.js returns for channel avatars.
    expect(normalizeThumbnailUrl("//yt3.ggpht.com/abc=s176-c-k-c0x00ffffff-no-rj-mo")).toBe(
      "https://yt3.ggpht.com/abc=s176-c-k-c0x00ffffff-no-rj-mo",
    );
    expect(normalizeThumbnailUrl("//yt3.googleusercontent.com/ytc/AIdro_x=s88")).toBe(
      "https://yt3.googleusercontent.com/ytc/AIdro_x=s88",
    );
  });

  it("leaves explicit https URLs untouched", () => {
    expect(normalizeThumbnailUrl("https://i.ytimg.com/vi/x/hqdefault.jpg")).toBe(
      "https://i.ytimg.com/vi/x/hqdefault.jpg",
    );
  });

  it("returns null for empty / missing input", () => {
    expect(normalizeThumbnailUrl(null)).toBeNull();
    expect(normalizeThumbnailUrl(undefined)).toBeNull();
    expect(normalizeThumbnailUrl("")).toBeNull();
    expect(normalizeThumbnailUrl("   ")).toBeNull();
  });
});

describe("pickThumbnail", () => {
  it("returns the highest-resolution thumbnail, normalized to https", () => {
    expect(
      pickThumbnail({
        default: { url: "//yt3.ggpht.com/small=s88" },
        high: { url: "//yt3.ggpht.com/big=s176" },
      }),
    ).toBe("https://yt3.ggpht.com/big=s176");
  });

  it("falls through an empty top-resolution URL to the next usable one", () => {
    expect(
      pickThumbnail({
        maxres: { url: "" },
        high: { url: "//host/x" },
      }),
    ).toBe("https://host/x");
  });

  it("returns null when no thumbnails are present", () => {
    expect(pickThumbnail({})).toBeNull();
  });
});
