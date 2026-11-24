import type { EngineEvent } from "@reticula/protocol";
import { describe, expect, test } from "vitest";
import { type PageSource, pumpPages } from "./page-pump.ts";

const styles = (sheet: number): EngineEvent => ({ kind: "stylesUpdated", sheet, styles: [] });

/** Pages named by their first byte; some intern a style while they are packed. */
function source(pages: readonly number[], styled: readonly number[] = []) {
  const due = [...pages];
  let aux: EngineEvent | undefined;
  return {
    taken: 0,
    takePage(): Uint8Array | undefined {
      const page = due.shift();
      if (page === undefined) {
        return undefined;
      }
      this.taken++;
      if (styled.includes(page)) {
        aux = styles(page);
      }
      return Uint8Array.of(page);
    },
    takeAuxEvent(): EngineEvent | undefined {
      const event = aux;
      aux = undefined;
      return event;
    },
  } satisfies PageSource & { taken: number };
}

function recorder() {
  const posted: string[] = [];
  return {
    posted,
    post(message: EngineEvent | ArrayBuffer, transfer?: Transferable[]) {
      if (message instanceof ArrayBuffer) {
        expect(transfer).toEqual([message]);
        posted.push(`page ${new Uint8Array(message)[0]}`);
      } else {
        posted.push(message.kind === "stylesUpdated" ? `styles ${message.sheet}` : message.kind);
      }
    },
  };
}

describe("posting pages", () => {
  test("sends all of them in the order they are due", () => {
    const { posted, post } = recorder();
    expect(pumpPages(source([1, 2, 3]), post, 4, () => 0)).toBe(false);
    expect(posted).toEqual(["page 1", "page 2", "page 3"]);
  });

  test("sends each page before packing the next one", () => {
    const pages = source([1, 2, 3]);
    const takenWhenPosted: number[] = [];
    pumpPages(
      pages,
      () => takenWhenPosted.push(pages.taken),
      4,
      () => 0,
    );
    expect(takenWhenPosted).toEqual([1, 2, 3]);
  });

  test("sends new styles before the page that needs them", () => {
    const { posted, post } = recorder();
    pumpPages(source([1, 2, 3], [2]), post, 4, () => 0);
    expect(posted).toEqual(["page 1", "styles 2", "page 2", "page 3"]);
  });

  test("stops when the slice is spent, and goes on where it stopped", () => {
    const { posted, post } = recorder();
    const pages = source([1, 2, 3, 4, 5]);
    let time = 0;
    const clock = () => time++;
    expect(pumpPages(pages, post, 2, clock)).toBe(true);
    expect(posted).toEqual(["page 1", "page 2"]);
    expect(pumpPages(pages, post, 2, clock)).toBe(true);
    expect(pumpPages(pages, post, 2, clock)).toBe(false);
    expect(posted).toEqual(["page 1", "page 2", "page 3", "page 4", "page 5"]);
  });

  test("sends at least one page however slow it was to pack", () => {
    const { posted, post } = recorder();
    let time = 0;
    expect(pumpPages(source([1, 2]), post, 4, () => (time += 100))).toBe(true);
    expect(posted).toEqual(["page 1"]);
  });

  test("sends an event that is left over without a page", () => {
    const { posted, post } = recorder();
    const left = { takePage: () => undefined, takeAuxEvent: source([]).takeAuxEvent };
    let pending: EngineEvent | undefined = styles(7);
    left.takeAuxEvent = () => {
      const event = pending;
      pending = undefined;
      return event;
    };
    expect(pumpPages(left, post)).toBe(false);
    expect(posted).toEqual(["styles 7"]);
  });
});
