import { Atlas } from "./atlas.ts";
import { Glyphs } from "./glyphs.ts";
import { Pipelines } from "./pipeline.ts";
import { fasterRaster } from "./raster.ts";

/** Why there is no device, as `RendererReason` has it. */
export class Refusal extends Error {
  readonly reason: "no-adapter" | "software-adapter" | "device-lost";

  constructor(reason: Refusal["reason"]) {
    super(reason);
    this.reason = reason;
  }
}

/** A device that was lost twice is not asked for again. */
const MOST_LOSSES = 2;

/** A device with what all the grids of a page share on it. */
export class Gpu {
  readonly device: GPUDevice;
  /** The adapter in words. */
  readonly adapter: string;
  readonly pipelines: Pipelines;
  readonly glyphs: Glyphs;
  /** Whether it was lost, or given back. */
  gone = false;
  /** Called once when the device is lost, however. */
  readonly listeners = new Set<() => void>();

  /** `lang` is the language of the document that the grids are in. */
  constructor(device: GPUDevice, adapter: string, lang: string) {
    this.device = device;
    this.adapter = adapter;
    this.pipelines = new Pipelines(device);
    this.glyphs = new Glyphs(
      (onEvict) => new Atlas(device, fasterRaster(device, lang), this.guard, onEvict),
    );
    this.glyphs.atlas.raster.context.canvas.addEventListener("contextlost", this.lose);
    // An invalid command leaves the picture as it was, and will do so again.
    device.addEventListener("uncapturederror", this.lose);
    void device.lost.then(this.lose);
  }

  /** Allocates something; if there is no memory for it, the device counts as lost. */
  readonly guard = <T>(allocate: () => T): T => {
    const { device } = this;
    device.pushErrorScope("out-of-memory");
    const allocated = allocate();
    device.popErrorScope().then(
      (error) => {
        if (error) {
          this.lose();
        }
      },
      // A lost device has no scopes, and says itself that it is lost.
      () => {},
    );
    return allocated;
  };

  readonly lose = (): void => {
    if (this.gone) {
      return;
    }
    losses++;
    this.close();
    for (const listener of this.listeners) {
      listener();
    }
  };

  close(): void {
    this.gone = true;
    if (shared?.device === this) {
      shared = null;
    }
    this.glyphs.atlas.raster.context.canvas.removeEventListener("contextlost", this.lose);
    this.device.removeEventListener("uncapturederror", this.lose);
    this.glyphs.atlas.destroy();
    this.device.destroy();
  }
}

/** A share in the page's device. */
export interface Lease {
  readonly gpu: Gpu;
  /** The last one to give up theirs closes the device. */
  readonly release: () => void;
}

interface Shared {
  readonly opening: Promise<Gpu>;
  device: Gpu | null;
  users: number;
}

let shared: Shared | null = null;
let losses = 0;

async function open(software: boolean): Promise<Gpu> {
  const adapter = await navigator.gpu?.requestAdapter();
  if (!adapter) {
    throw new Refusal("no-adapter");
  }
  const { info } = adapter;
  const { isFallbackAdapter } = adapter as { isFallbackAdapter?: boolean };
  if ((info.isFallbackAdapter ?? isFallbackAdapter) && !software) {
    throw new Refusal("software-adapter");
  }
  const { maxTextureDimension2D } = adapter.limits;
  // A canvas across several screens may be larger than what is guaranteed.
  const device = await adapter.requestDevice({ requiredLimits: { maxTextureDimension2D } });
  try {
    const name = [info.vendor, info.architecture, info.description].join(" ").trim();
    const gpu = new Gpu(device, name, document.documentElement.lang);
    await gpu.pipelines.prepare(navigator.gpu.getPreferredCanvasFormat());
    return gpu;
  } catch (error) {
    device.destroy();
    throw error;
  }
}

/** The page's device, opened if there is none; a software adapter only if that is wanted. */
export async function acquire(software: boolean, onLost: () => void): Promise<Lease> {
  if (losses >= MOST_LOSSES) {
    throw new Refusal("device-lost");
  }
  const share: Shared = shared ?? { opening: open(software), device: null, users: 0 };
  shared = share;
  share.users++;
  let released = false;
  const release = () => {
    if (!released) {
      released = true;
      share.users--;
      share.device?.listeners.delete(onLost);
      if (share.users === 0 && share.device && !share.device.gone) {
        share.device.close();
      }
    }
  };
  try {
    const gpu = await share.opening;
    share.device = gpu;
    if (gpu.gone) {
      throw new Refusal("device-lost");
    }
    gpu.listeners.add(onLost);
    return { gpu, release };
  } catch (error) {
    if (shared === share) {
      shared = null;
    }
    throw error;
  }
}
