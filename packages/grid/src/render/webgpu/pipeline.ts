// What the GPU is told: one kind of quad, plain or a copy of a part of the atlas,
// so that the order of instances alone decides what is painted over what.

// The type library has the interfaces of WebGPU but not its constants.
export const BUFFER = { mapRead: 0x1, copySrc: 0x4, copyDst: 0x8, vertex: 0x20, uniform: 0x40 };
export const TEXTURE = { copySrc: 0x1, copyDst: 0x2, binding: 0x4, attachment: 0x10 };
const STAGE = { vertex: 0x1, fragment: 0x2 };

/** Position, size, place in the atlas, colour and flags: see `InstanceWriter`. */
export const INSTANCE_BYTES = 24;

/** Size of the screen: two floats. */
export const SCREEN_BYTES = 8;

/** An origin, a scale, the rows that are shown, and what dynamic offsets have to be multiples of. */
export const PART_BYTES = 24;
export const PART_STRIDE = 256;

// The position of a quad is whole pixels, and so is that of a texel: texels are
// counted in integers, and none is ever sampled between two. But for parts that
// are stretched, which stand for ones that are being made.
const SHADER = /* wgsl */ `
struct Screen { size: vec2f };
struct Part { origin: vec2f, scale: vec2f, rows: vec2u };

const STRETCHED: i32 = 256;

@group(0) @binding(0) var<uniform> screen: Screen;
@group(0) @binding(1) var atlas: texture_2d_array<f32>;
@group(0) @binding(2) var between: sampler;
@group(1) @binding(0) var<uniform> part: Part;

struct Quad {
  @builtin(position) position: vec4f,
  @location(0) @interpolate(flat) color: vec4f,
  @location(1) @interpolate(flat) texel: vec2i,
  @location(2) @interpolate(flat) layer: i32,
  @location(3) spot: vec2f,
};

@vertex
fn place(
  @builtin(vertex_index) index: u32,
  @location(0) at: vec2i,
  @location(1) box: vec4u,
  @location(2) color: vec4f,
  @location(3) flags: u32,
) -> Quad {
  let row = flags & 0x7fu;
  let hidden = row != 0x7fu && (row < part.rows.x || row >= part.rows.y);
  let size = select(vec2i(box.xy), vec2i(), hidden);
  let corner = size * vec2i(i32(index & 1u), i32(index >> 1u));
  let point = part.origin + vec2f(at + corner) * part.scale;
  var quad: Quad;
  quad.position = vec4f(
    point.x / screen.size.x * 2.0 - 1.0,
    1.0 - point.y / screen.size.y * 2.0,
    0.0,
    1.0,
  );
  quad.color = vec4f(color.rgb * color.a, color.a);
  quad.texel = vec2i(box.zw) - at - vec2i(part.origin);
  quad.layer = select(-1, i32(flags >> 8u), (flags & 0x80u) != 0u);
  if (quad.layer >= 0 && any(part.scale != vec2f(1.0))) {
    quad.layer += STRETCHED;
  }
  quad.spot = vec2f(vec2i(box.zw) + corner);
  return quad;
}

@fragment
fn paint(quad: Quad) -> @location(0) vec4f {
  if (quad.layer < 0) {
    return quad.color;
  }
  if (quad.layer >= STRETCHED) {
    let spot = quad.spot / vec2f(textureDimensions(atlas));
    return textureSampleLevel(atlas, between, spot, quad.layer - STRETCHED, 0.0);
  }
  return textureLoad(atlas, vec2i(quad.position.xy) + quad.texel, quad.layer, 0);
}
`;

/** The quad pipeline, for each format of what it is drawn into. */
export class Pipelines {
  readonly screenLayout: GPUBindGroupLayout;
  readonly partLayout: GPUBindGroupLayout;
  readonly sampler: GPUSampler;
  #device: GPUDevice;
  #module: GPUShaderModule;
  #layout: GPUPipelineLayout;
  #made = new Map<GPUTextureFormat, GPURenderPipeline>();

  constructor(device: GPUDevice) {
    this.#device = device;
    this.#module = device.createShaderModule({ code: SHADER });
    this.screenLayout = device.createBindGroupLayout({
      entries: [
        { binding: 0, visibility: STAGE.vertex, buffer: {} },
        { binding: 1, visibility: STAGE.fragment, texture: { viewDimension: "2d-array" } },
        { binding: 2, visibility: STAGE.fragment, sampler: {} },
      ],
    });
    this.sampler = device.createSampler({ magFilter: "linear", minFilter: "linear" });
    this.partLayout = device.createBindGroupLayout({
      entries: [
        {
          binding: 0,
          visibility: STAGE.vertex,
          buffer: { hasDynamicOffset: true, minBindingSize: PART_BYTES },
        },
      ],
    });
    this.#layout = device.createPipelineLayout({
      bindGroupLayouts: [this.screenLayout, this.partLayout],
    });
  }

  /** Compiles a pipeline without holding up the first frame that needs it. */
  async prepare(format: GPUTextureFormat): Promise<void> {
    if (!this.#made.has(format)) {
      this.#made.set(
        format,
        await this.#device.createRenderPipelineAsync(this.#descriptor(format)),
      );
    }
  }

  get(format: GPUTextureFormat): GPURenderPipeline {
    let pipeline = this.#made.get(format);
    if (!pipeline) {
      pipeline = this.#device.createRenderPipeline(this.#descriptor(format));
      this.#made.set(format, pipeline);
    }
    return pipeline;
  }

  #descriptor(format: GPUTextureFormat): GPURenderPipelineDescriptor {
    // What a 2D canvas does with everything it paints, on premultiplied colours.
    const over = { srcFactor: "one", dstFactor: "one-minus-src-alpha" } as const;
    return {
      layout: this.#layout,
      vertex: {
        module: this.#module,
        entryPoint: "place",
        buffers: [
          {
            arrayStride: INSTANCE_BYTES,
            stepMode: "instance",
            attributes: [
              { shaderLocation: 0, offset: 0, format: "sint32x2" },
              { shaderLocation: 1, offset: 8, format: "uint16x4" },
              { shaderLocation: 2, offset: 16, format: "unorm8x4" },
              { shaderLocation: 3, offset: 20, format: "uint32" },
            ],
          },
        ],
      },
      fragment: {
        module: this.#module,
        entryPoint: "paint",
        targets: [{ format, blend: { color: over, alpha: over } }],
      },
      primitive: { topology: "triangle-strip" },
    };
  }
}
