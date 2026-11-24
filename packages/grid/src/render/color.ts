/**
 * A colour as four bytes, red in the lowest: what an rgba8 texture holds.
 * Alpha is straight, not multiplied into the others.
 */
export type PackedColor = number;

export function packColor(red: number, green: number, blue: number, alpha = 255): PackedColor {
  return (red | (green << 8) | (blue << 16) | (alpha << 24)) >>> 0;
}

export function alphaOf(color: PackedColor): number {
  return color >>> 24;
}

const channel = (value: number) => Math.min(255, Math.max(0, Math.round(value)));

/** Reads a colour the way a 2D context writes out its `fillStyle`, or `null`. */
export function parseSerialized(text: string): PackedColor | null {
  const hex = /^#([0-9a-f]{6})$/i.exec(text)?.[1];
  if (hex !== undefined) {
    const rgb = Number.parseInt(hex, 16);
    return packColor((rgb >> 16) & 0xff, (rgb >> 8) & 0xff, rgb & 0xff);
  }
  const body = /^(rgba?|color)\((.*)\)$/i.exec(text);
  if (!body) {
    return null;
  }
  const parts = (body[2] ?? "").split(/[\s,/]+/).filter((part) => part !== "");
  const wide = body[1] === "color";
  if (wide && parts.shift() !== "srgb") {
    return null;
  }
  const [red, green, blue, alpha = 1] = parts.map(Number);
  if (red === undefined || green === undefined || blue === undefined) {
    return null;
  }
  if (Number.isNaN(red + green + blue + alpha)) {
    return null;
  }
  const unit = wide ? 255 : 1;
  return packColor(
    channel(red * unit),
    channel(green * unit),
    channel(blue * unit),
    channel(alpha * 255),
  );
}

/** What a 2D context makes of a CSS colour, without reading any pixels back. */
export function serializer(context: CanvasRenderingContext2D): (css: string) => string {
  return (css) => {
    context.fillStyle = "#000000";
    context.fillStyle = css;
    return String(context.fillStyle);
  };
}

/** CSS colours in packed form, each resolved once. */
export class Colors {
  #serialize: (css: string) => string;
  #packed = new Map<string, PackedColor>();

  constructor(serialize: (css: string) => string) {
    this.#serialize = serialize;
  }

  parse(css: string): PackedColor {
    let packed = this.#packed.get(css);
    if (packed === undefined) {
      packed =
        parseSerialized(this.#serialize(css)) ??
        // Colours outside sRGB are written out in their own space.
        parseSerialized(this.#serialize(`color-mix(in srgb, ${css}, ${css})`)) ??
        packColor(0, 0, 0);
      this.#packed.set(css, packed);
    }
    return packed;
  }
}

const written = new Map<PackedColor, string>();

/** A packed colour as CSS. */
export function cssColor(color: PackedColor): string {
  let css = written.get(color);
  if (css === undefined) {
    const bytes = [color & 0xff, (color >>> 8) & 0xff, (color >>> 16) & 0xff, color >>> 24];
    const opaque = bytes[3] === 255;
    css = `#${bytes
      .slice(0, opaque ? 3 : 4)
      .map((byte) => byte.toString(16).padStart(2, "0"))
      .join("")}`;
    written.set(color, css);
  }
  return css;
}
