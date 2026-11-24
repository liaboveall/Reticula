import { useId } from "react";
import { t } from "../i18n/index.ts";
import { KERNEL, RENDERER_NAMES } from "./names.ts";

interface Box {
  readonly x: number;
  readonly y: number;
  readonly w: number;
  readonly h: number;
  readonly kind: "place" | "thread" | "part" | "apart";
  readonly title: string;
  readonly lines?: readonly string[];
}

/** A line from one point to another, with an arrow at its end or at both. */
interface Arrow {
  readonly at: `${number},${number} ${number},${number}`;
  readonly both?: boolean;
  /** Stands for "the same code", not for data. */
  readonly same?: boolean;
}

interface Label {
  readonly x: number;
  readonly y: number;
  readonly anchor: "start" | "middle" | "end";
  readonly text: string;
}

interface Layout {
  readonly name: "wide" | "tall";
  readonly width: number;
  readonly height: number;
  readonly boxes: readonly Box[];
  readonly arrows: readonly Arrow[];
  readonly labels: readonly Label[];
}

/** What the boxes say, wherever they are. */
function words() {
  return {
    tab: t("Browser tab"),
    main: t("Main thread"),
    interface: { title: t("Interface") },
    input: { title: t("Input") },
    scene: { title: t("Scene") },
    canvas: { title: RENDERER_NAMES.canvas2d },
    webgpu: { title: RENDERER_NAMES.webgpu },
    worker: t("Engine worker"),
    kernel: { title: t("Kernel and formulas"), lines: [KERNEL] },
    sync: { title: t("Sync client") },
    xlsx: { title: "XLSX" },
    files: { title: "OPFS", lines: [t("Write-ahead log"), t("Two snapshots")] },
    server: t("Server"),
    actor: { title: t("Sync server"), lines: [t("An actor for each workbook")] },
    postgres: { title: "PostgreSQL", lines: [t("Stored before acknowledged")] },
    snapshots: { title: t("Snapshots") },
    draft: { title: t("AI draft branches") },
    simulator: t("Deterministic simulator"),
    simulated: [t("A simulated network"), t("Faults from a seed")],
    commands: t("commands by ID"),
    pages: t("pages of cells"),
    logged: t("logged before it is sent"),
    socket: "WebSocket",
    transactions: t("transactions"),
    commits: t("ordered commits"),
    merge: t("fork, review, merge"),
    same: t("same code"),
  };
}

const label = (x: number, y: number, text: string, anchor: Label["anchor"] = "middle"): Label => ({
  x,
  y,
  anchor,
  text,
});

function wide(w: ReturnType<typeof words>): Layout {
  return {
    name: "wide",
    width: 1120,
    height: 514,
    boxes: [
      { x: 0, y: 0, w: 644, h: 392, kind: "place", title: w.tab },
      { x: 18, y: 40, w: 232, h: 334, kind: "thread", title: w.main },
      { x: 32, y: 78, w: 204, h: 38, kind: "part", ...w.interface },
      { x: 32, y: 126, w: 204, h: 38, kind: "part", ...w.input },
      { x: 32, y: 194, w: 204, h: 38, kind: "part", ...w.scene },
      { x: 32, y: 314, w: 96, h: 38, kind: "part", ...w.canvas },
      { x: 140, y: 314, w: 96, h: 38, kind: "part", ...w.webgpu },
      { x: 394, y: 40, w: 232, h: 208, kind: "thread", title: w.worker },
      { x: 408, y: 78, w: 204, h: 60, kind: "part", ...w.kernel },
      { x: 408, y: 148, w: 204, h: 38, kind: "part", ...w.sync },
      { x: 408, y: 196, w: 204, h: 38, kind: "part", ...w.xlsx },
      { x: 394, y: 296, w: 232, h: 78, kind: "thread", ...w.files },
      { x: 768, y: 0, w: 352, h: 392, kind: "place", title: w.server },
      { x: 784, y: 40, w: 320, h: 66, kind: "thread", ...w.actor },
      { x: 784, y: 172, w: 200, h: 62, kind: "thread", ...w.postgres },
      { x: 998, y: 172, w: 106, h: 62, kind: "thread", ...w.snapshots },
      { x: 784, y: 334, w: 320, h: 40, kind: "thread", ...w.draft },
      {
        x: 0,
        y: 432,
        w: 1120,
        h: 80,
        kind: "apart",
        title: w.simulator,
        lines: w.simulated,
      },
    ],
    arrows: [
      { at: "250,100 394,100" },
      { at: "394,213 250,213" },
      { at: "80,232 80,314" },
      { at: "188,232 188,314" },
      { at: "510,248 510,296", both: true },
      { at: "626,62 784,62" },
      { at: "784,90 626,90" },
      { at: "884,106 884,172" },
      { at: "1051,106 1051,172", both: true },
      { at: "991,106 991,334", both: true },
      { at: "322,432 322,392", same: true },
      { at: "944,432 944,392", same: true },
    ],
    labels: [
      label(322, 90, w.commands),
      label(322, 203, w.pages),
      label(500, 277, w.logged, "end"),
      label(706, 30, w.socket),
      label(706, 53, w.transactions),
      label(706, 109, w.commits),
      label(981, 290, w.merge, "end"),
      label(332, 417, w.same, "start"),
      label(954, 417, w.same, "start"),
    ],
  };
}

function tall(w: ReturnType<typeof words>): Layout {
  return {
    name: "tall",
    width: 360,
    height: 1172,
    boxes: [
      { x: 0, y: 0, w: 360, h: 650, kind: "place", title: w.tab },
      { x: 14, y: 38, w: 332, h: 250, kind: "thread", title: w.main },
      { x: 28, y: 74, w: 304, h: 38, kind: "part", ...w.interface },
      { x: 28, y: 118, w: 304, h: 38, kind: "part", ...w.input },
      { x: 28, y: 162, w: 304, h: 38, kind: "part", ...w.scene },
      { x: 28, y: 236, w: 146, h: 38, kind: "part", ...w.canvas },
      { x: 186, y: 236, w: 146, h: 38, kind: "part", ...w.webgpu },
      { x: 14, y: 340, w: 332, h: 190, kind: "thread", title: w.worker },
      { x: 28, y: 376, w: 304, h: 58, kind: "part", ...w.kernel },
      { x: 28, y: 440, w: 304, h: 38, kind: "part", ...w.sync },
      { x: 28, y: 484, w: 304, h: 36, kind: "part", ...w.xlsx },
      { x: 14, y: 566, w: 332, h: 74, kind: "thread", ...w.files },
      { x: 0, y: 714, w: 360, h: 338, kind: "place", title: w.server },
      { x: 14, y: 752, w: 332, h: 62, kind: "thread", ...w.actor },
      { x: 14, y: 866, w: 196, h: 62, kind: "thread", ...w.postgres },
      { x: 224, y: 866, w: 122, h: 62, kind: "thread", ...w.snapshots },
      { x: 14, y: 998, w: 332, h: 40, kind: "thread", ...w.draft },
      {
        x: 0,
        y: 1092,
        w: 360,
        h: 80,
        kind: "apart",
        title: w.simulator,
        lines: w.simulated,
      },
    ],
    arrows: [
      { at: "101,200 101,236" },
      { at: "259,200 259,236" },
      { at: "60,288 60,340" },
      { at: "210,340 210,288" },
      { at: "60,530 60,566", both: true },
      { at: "60,650 60,714" },
      { at: "210,714 210,650" },
      { at: "80,814 80,866" },
      { at: "285,814 285,866", both: true },
      { at: "217,814 217,998", both: true },
      { at: "60,1092 60,1052", same: true },
    ],
    labels: [
      label(70, 318, w.commands, "start"),
      label(220, 318, w.pages, "start"),
      label(70, 552, w.logged, "start"),
      label(70, 687, w.transactions, "start"),
      label(220, 687, w.commits, "start"),
      label(207, 968, w.merge, "end"),
      label(70, 1077, w.same, "start"),
    ],
  };
}

/** The end of an arrow from one point to another. */
function head(from: string, to: string): string {
  const [x0 = 0, y0 = 0] = from.split(",").map(Number);
  const [x = 0, y = 0] = to.split(",").map(Number);
  const [dx, dy] = [8 * Math.sign(x - x0), 8 * Math.sign(y - y0)];
  return `${x},${y} ${x - dx - dy / 2},${y - dy - dx / 2} ${x - dx + dy / 2},${y - dy + dx / 2}`;
}

function Drawing({ layout, title }: { readonly layout: Layout; readonly title: string }) {
  const id = useId();
  return (
    <svg
      className={`landing-diagram ${layout.name}`}
      viewBox={`-1 -1 ${layout.width + 2} ${layout.height + 2}`}
      role="img"
      aria-labelledby={id}
    >
      <title id={id}>{title}</title>
      {layout.boxes.map(({ x, y, w, h, kind, title: name, lines }) => (
        <g key={`${x} ${y} ${w}`}>
          <rect x={x} y={y} width={w} height={h} rx={kind === "place" ? 8 : 5} className={kind} />
          <text x={x + 14} y={y + 24} className={kind === "part" ? "part-title" : "title"}>
            {name}
          </text>
          {lines?.map((line, index) => (
            <text key={line} x={x + 14} y={y + 45 + index * 18}>
              {line}
            </text>
          ))}
        </g>
      ))}
      {layout.arrows.map(({ at, both, same }) => {
        const [from = "", to = ""] = at.split(" ");
        return (
          <g key={at} className={same ? "same" : "flow"}>
            <polyline points={at} />
            {!same && <polygon points={head(from, to)} />}
            {both && <polygon points={head(to, from)} />}
          </g>
        );
      })}
      {layout.labels.map(({ x, y, anchor, text }) => (
        <text key={`${x} ${y}`} x={x} y={y} textAnchor={anchor} className="edge">
          {text}
        </text>
      ))}
    </svg>
  );
}

export function Architecture() {
  const w = words();
  const title = t("What flows between the browser, the server and the simulator");
  return (
    <section id="architecture" className="landing-section" aria-labelledby="architecture-heading">
      <h2 id="architecture-heading">{t("Architecture")}</h2>
      <p className="landing-intro">
        {t("One kernel without I/O, in the browser, the server, the tests and the simulator.")}
      </p>
      <Drawing layout={wide(w)} title={title} />
      <Drawing layout={tall(w)} title={title} />
      <h3>{t("What an edit goes through")}</h3>
      <ol className="landing-steps">
        <li>{t("A command names the cell by the identity of its row and column.")}</li>
        <li>{t("The worker applies it, logs it, and recalculates in slices of 4 ms.")}</li>
        <li>{t("Changed pages of cells come back, and the next frame draws them.")}</li>
        <li>{t("In a shared workbook the server orders it, stores it, then tells everyone.")}</li>
        <li>{t("Edits not yet sent are taken back, and replayed after it.")}</li>
      </ol>
    </section>
  );
}
