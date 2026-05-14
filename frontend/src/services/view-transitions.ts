/**
 * MPA View Transitions — directional animation support.
 *
 * Uses the cross-document View Transition API (Chrome 126+) to assign
 * transition types based on navigation direction (forward/backward).
 * Falls back gracefully in unsupported browsers (the CSS and events
 * simply don't fire).
 */

function getDepth(url: URL): number {
  return url.pathname.split("/").filter(Boolean).length;
}

function determineType(from: URL, to: URL): string {
  const fromDepth = getDepth(from);
  const toDepth = getDepth(to);
  if (toDepth > fromDepth) return "forward";
  if (toDepth < fromDepth) return "backward";
  return "same-level";
}

// Feature detect: only set up if the browser supports cross-document
// view transitions (Chrome 126+).
if ("PageRevealEvent" in window) {
  window.addEventListener("pageswap", (e) => {
    const evt = e as unknown as {
      viewTransition: ViewTransition | null;
      activation: { from: NavigationHistoryEntry; entry: NavigationHistoryEntry } | null;
    };
    if (evt.viewTransition && evt.activation) {
      const fromUrl = evt.activation.from?.url;
      const toUrl = evt.activation.entry?.url;
      if (fromUrl && toUrl) {
        const parsedTo = new URL(toUrl);
        evt.viewTransition.types.add(determineType(new URL(fromUrl), parsedTo));

        // Hero transition: if navigating to a video page, tag the
        // clicked video card's thumbnail so it animates into the player.
        const videoMatch = parsedTo.pathname.match(/^\/child\/video\/(.+)$/);
        if (videoMatch) {
          const videoId = decodeURIComponent(videoMatch[1]);
          const cards = document.querySelectorAll("hometube-video-card");
          for (const card of cards) {
            if (card.getAttribute("video-id") === videoId) {
              (card as HTMLElement).style.viewTransitionName = "video-hero";
              break;
            }
          }
        }
      }
    }
  });

  window.addEventListener("pagereveal", (e) => {
    const evt = e as unknown as { viewTransition: ViewTransition | null };
    if (evt.viewTransition) {
      const activation = (navigation as { activation?: { from?: NavigationHistoryEntry } })
        .activation;
      const fromUrl = activation?.from?.url;
      if (fromUrl) {
        evt.viewTransition.types.add(determineType(new URL(fromUrl), new URL(document.URL)));
      }
    }
  });
}
