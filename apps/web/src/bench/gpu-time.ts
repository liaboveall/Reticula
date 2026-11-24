// How long the GPU takes for a frame. The renderers know nothing of it: while a
// timer is there, what they ask WebGPU for is asked for with timestamps.

import { type Summary, summarize } from "./stats.ts";

// The type library that comes with TypeScript has the interfaces of WebGPU but
// not its constants.
const MAP_READ = 0x1;
const COPY_SRC = 0x4;
const COPY_DST = 0x8;
const QUERY_RESOLVE = 0x200;
/** Two timestamps of eight bytes. */
const BYTES = 16;
/** Buffers for timestamps on their way back; frames that find none are not timed. */
const MOST_PENDING = 8;

interface Slot {
  readonly queries: GPUQuerySet;
  readonly resolved: GPUBuffer;
  readonly readable: GPUBuffer;
}

const adapters = () => GPUAdapter.prototype;
const devices = () => GPUDevice.prototype;
const encoders = () => GPUCommandEncoder.prototype;
const queues = () => GPUQueue.prototype;

/** Times the first pass of every encoder of devices that are opened from now on. */
export class GpuTimer {
  #samples: number[] = [];
  #restore: () => void;
  #free: Slot[] = [];
  #slots = 0;
  #reading: Promise<void>[] = [];
  #isOver = false;

  constructor() {
    const { requestDevice } = adapters();
    const { createCommandEncoder } = devices();
    const { beginRenderPass, finish } = encoders();
    const { submit } = queues();
    const timer = this;
    const deviceOf = new WeakMap<GPUCommandEncoder, GPUDevice>();
    const slotOf = new WeakMap<GPUCommandEncoder, Slot>();
    let finished: Slot[] = [];
    adapters().requestDevice = function (descriptor = {}) {
      const requiredFeatures: GPUFeatureName[] = this.features.has("timestamp-query")
        ? ["timestamp-query"]
        : [];
      return requestDevice.call(this, { ...descriptor, requiredFeatures });
    };
    devices().createCommandEncoder = function (descriptor) {
      const encoder = createCommandEncoder.call(this, descriptor);
      if (this.features.has("timestamp-query")) {
        deviceOf.set(encoder, this);
      }
      return encoder;
    };
    encoders().beginRenderPass = function (descriptor) {
      const device = deviceOf.get(this);
      const slot = device && !slotOf.has(this) ? timer.#take(device) : null;
      if (!slot) {
        return beginRenderPass.call(this, descriptor);
      }
      slotOf.set(this, slot);
      return beginRenderPass.call(this, {
        ...descriptor,
        timestampWrites: {
          querySet: slot.queries,
          beginningOfPassWriteIndex: 0,
          endOfPassWriteIndex: 1,
        },
      });
    };
    encoders().finish = function (descriptor) {
      const slot = slotOf.get(this);
      if (slot) {
        this.resolveQuerySet(slot.queries, 0, 2, slot.resolved, 0);
        this.copyBufferToBuffer(slot.resolved, 0, slot.readable, 0, BYTES);
        finished.push(slot);
      }
      return finish.call(this, descriptor);
    };
    queues().submit = function (buffers) {
      submit.call(this, buffers);
      const submitted = finished;
      finished = [];
      for (const slot of submitted) {
        timer.#reading.push(timer.#read(slot));
      }
    };
    this.#restore = () => {
      adapters().requestDevice = requestDevice;
      devices().createCommandEncoder = createCommandEncoder;
      Object.assign(encoders(), { beginRenderPass, finish });
      queues().submit = submit;
    };
  }

  /** Stops, and says what the passes took once all of them have reported. */
  async finish(): Promise<Summary | null> {
    this.#isOver = true;
    this.#restore();
    await Promise.allSettled(this.#reading);
    return this.#samples.length > 0 ? summarize(this.#samples) : null;
  }

  #take(device: GPUDevice): Slot | null {
    const free = this.#free.pop();
    if (free || this.#isOver || this.#slots >= MOST_PENDING) {
      return free ?? null;
    }
    this.#slots++;
    return {
      queries: device.createQuerySet({ type: "timestamp", count: 2 }),
      resolved: device.createBuffer({ size: BYTES, usage: QUERY_RESOLVE | COPY_SRC }),
      readable: device.createBuffer({ size: BYTES, usage: MAP_READ | COPY_DST }),
    };
  }

  async #read(slot: Slot): Promise<void> {
    await slot.readable.mapAsync(MAP_READ);
    const [begin = 0n, end = 0n] = new BigUint64Array(slot.readable.getMappedRange());
    slot.readable.unmap();
    this.#free.push(slot);
    // Clocks that are set back in between say nothing.
    if (end > begin) {
      this.#samples.push(Number(end - begin) / 1e6);
    }
  }
}
