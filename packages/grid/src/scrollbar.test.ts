import { afterEach, expect, test, vi } from "vitest";
import { Scrollbar } from "./scrollbar.ts";

class FakeElement extends EventTarget {
  className = "";
  hidden = false;
  clientHeight = 100;
  dataset: Record<string, string> = {};
  style: Record<string, string> = {};
  children: FakeElement[] = [];
  setPointerCapture = vi.fn();

  append(child: FakeElement): void {
    this.children.push(child);
  }

  getBoundingClientRect() {
    return { left: 0, top: 0 };
  }
}

function open() {
  vi.stubGlobal("document", { createElement: () => new FakeElement() });
  const onScroll = vi.fn();
  const bar = new Scrollbar("vertical", onScroll);
  bar.update(1000, 100, 0);
  const [thumb] = (bar.element as unknown as FakeElement).children;
  if (!thumb) {
    throw new Error("the scrollbar has no thumb");
  }
  const send = (type: string, clientY = 0) =>
    thumb.dispatchEvent(Object.assign(new Event(type), { button: 0, pointerId: 1, clientY }));
  return { bar, thumb, send, onScroll };
}

afterEach(() => {
  vi.unstubAllGlobals();
});

test("a thumb is dragged until its capture is lost", () => {
  const { bar, send, onScroll } = open();
  expect(bar.isDragging).toBe(false);
  send("pointerdown");
  expect(bar.isDragging).toBe(true);
  send("pointermove", 10);
  expect(onScroll).toHaveBeenCalledOnce();
  send("lostpointercapture");
  expect(bar.isDragging).toBe(false);
});

test("a thumb that cannot have the pointer is not dragged, which would never end", () => {
  const { bar, thumb, send, onScroll } = open();
  thumb.setPointerCapture.mockImplementation(() => {
    throw new DOMException("No active pointer with the given id is found.", "NotFoundError");
  });
  send("pointerdown");
  expect(bar.isDragging).toBe(false);
  send("pointermove", 10);
  expect(onScroll).not.toHaveBeenCalled();
});
