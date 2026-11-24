/** The style sheets that have been asked for, by their addresses. */
const sheets = new Map<string, Promise<void>>();

function loadSheet(address: string): Promise<void> {
  let loading = sheets.get(address);
  if (!loading) {
    loading = new Promise((resolve, reject) => {
      const link = document.createElement("link");
      link.rel = "stylesheet";
      link.crossOrigin = "";
      link.href = address;
      link.onload = () => resolve();
      link.onerror = () => {
        link.remove();
        sheets.delete(address);
        reject(new Error(`Unable to preload CSS for ${link.href}`));
      };
      document.head.append(link);
    });
    sheets.set(address, loading);
  }
  return loading;
}

/** Tells who listens. Unless one of them prevents it, the error is thrown as well. */
function report(payload: unknown): undefined {
  const event = Object.assign(new Event("vite:preloadError", { cancelable: true }), { payload });
  window.dispatchEvent(event);
  if (!event.defaultPrevented) {
    throw payload;
  }
}

/**
 * What every `import()` of a chunk is turned into: the style sheets that belong
 * to the chunk are loaded first, and all of it is reported when it fails. It
 * takes the place of Vite's own module (`build/load-chunk.ts`), and does of
 * what that one can do what this app has a use for.
 */
export async function loadChunk<Chunk>(
  load: () => Promise<Chunk>,
  files: readonly string[] = [],
): Promise<Chunk | undefined> {
  const sheetsLoaded = await Promise.allSettled(files.map((file) => loadSheet(`/${file}`)));
  for (const outcome of sheetsLoaded) {
    if (outcome.status === "rejected") {
      report(outcome.reason);
    }
  }
  return load().catch(report);
}
