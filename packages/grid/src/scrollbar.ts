import { scrollAtThumb, thumb } from "./layout.ts";

/** The shortest a thumb gets, so that it can always be grabbed. */
const MIN_THUMB = 32;

/**
 * A scrollbar of the grid's own.
 *
 * The browser's scrolling is not used: a million rows are taller than the
 * largest element some browsers can lay out. The thumb's travel is mapped
 * onto the scroll range in proportion instead.
 */
export class Scrollbar {
  readonly element: HTMLDivElement;
  #thumb: HTMLDivElement;
  #vertical: boolean;
  #onScroll: (position: number) => void;
  #content = 0;
  #viewport = 0;
  #position = 0;
  /** While the thumb is dragged: where on the thumb it was grabbed. */
  #grip: number | null = null;

  constructor(orientation: "vertical" | "horizontal", onScroll: (position: number) => void) {
    this.#vertical = orientation === "vertical";
    this.#onScroll = onScroll;

    this.element = document.createElement("div");
    this.element.className = "reticula-scrollbar";
    this.element.dataset.orientation = orientation;
    this.#thumb = document.createElement("div");
    this.#thumb.className = "reticula-scrollbar-thumb";
    this.element.append(this.#thumb);

    this.#thumb.addEventListener("pointerdown", this.#grab);
    this.#thumb.addEventListener("pointermove", this.#drag);
    // The browser takes the capture back when the pointer is lifted or
    // cancelled, and whenever else it sees fit; that ends the drag.
    this.#thumb.addEventListener("lostpointercapture", this.#release);
    this.element.addEventListener("pointerdown", this.#page);
  }

  get isDragging(): boolean {
    return this.#grip !== null;
  }

  /** Shows where `position` is within `content`, of which `viewport` is visible. */
  update(content: number, viewport: number, position: number): void {
    this.#content = content;
    this.#viewport = viewport;
    this.#position = position;
    const placed = thumb(this.#track(), content, viewport, position, MIN_THUMB);
    if (!placed) {
      this.#thumb.hidden = true;
      return;
    }
    this.#thumb.hidden = false;
    const style = this.#thumb.style;
    if (this.#vertical) {
      style.top = `${placed.offset}px`;
      style.height = `${placed.length}px`;
    } else {
      style.left = `${placed.offset}px`;
      style.width = `${placed.length}px`;
    }
  }

  #track(): number {
    return this.#vertical ? this.element.clientHeight : this.element.clientWidth;
  }

  #along(event: PointerEvent): number {
    const bounds = this.element.getBoundingClientRect();
    return this.#vertical ? event.clientY - bounds.top : event.clientX - bounds.left;
  }

  #placed(): { offset: number; length: number } | null {
    return thumb(this.#track(), this.#content, this.#viewport, this.#position, MIN_THUMB);
  }

  #grab = (event: PointerEvent): void => {
    const placed = this.#placed();
    if (event.button !== 0 || !placed) {
      return;
    }
    event.preventDefault();
    event.stopPropagation();
    // Without the capture nothing would say when the drag has ended. It is
    // refused if the pointer is gone by now.
    try {
      this.#thumb.setPointerCapture(event.pointerId);
    } catch {
      return;
    }
    this.#grip = this.#along(event) - placed.offset;
    this.#thumb.dataset.dragging = "";
  };

  #drag = (event: PointerEvent): void => {
    const placed = this.#placed();
    if (this.#grip === null || !placed) {
      return;
    }
    const offset = this.#along(event) - this.#grip;
    this.#onScroll(
      scrollAtThumb(this.#track(), this.#content, this.#viewport, offset, placed.length),
    );
  };

  #release = (): void => {
    this.#grip = null;
    delete this.#thumb.dataset.dragging;
  };

  /** A press on the track moves one screenful towards the pointer. */
  #page = (event: PointerEvent): void => {
    const placed = this.#placed();
    if (event.button !== 0 || event.target !== this.element || !placed) {
      return;
    }
    event.preventDefault();
    const direction = this.#along(event) < placed.offset ? -1 : 1;
    this.#onScroll(this.#position + direction * this.#viewport);
  };
}
