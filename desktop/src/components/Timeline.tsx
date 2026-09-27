// The current waveform and the firmware-event lane, on one shared time axis.
//
// They are one canvas rather than two stacked charts on purpose. The shared axis is the
// product — the whole question this tool answers is "what was the firmware doing when that
// spike happened" — and two charts that each own a time range are two charts that will
// eventually disagree by a pixel and make the answer wrong.
//
// Two rendering rules that are not cosmetic:
//
//   * An empty bucket is a gap, never zero current. `Bucket.count == 0` means nothing was
//     sampled there; drawing it at 0 uA invents a measurement.
//   * Every bucket's min and max are drawn, not just the mean. A 90 mA peak that falls
//     between two pixels is exactly the peak someone opened this window to find.

import { useEffect, useMemo, useRef, useState } from "react";
import type { Bucket, EventMark, Gap } from "../api";
import { axisTime, eventColor, uA } from "../format";

export interface View {
  start_ns: number;
  end_ns: number;
}

export interface Selection {
  start_ns: number;
  end_ns: number;
}

interface Props {
  view: View;
  full: View;
  buckets: Bucket[];
  marks: EventMark[];
  gaps: Gap[];
  selection: Selection | null;
  onView: (v: View) => void;
  onSelection: (s: Selection | null) => void;
  /** Reported back so the parent can request exactly one bucket per pixel. */
  onWidth: (px: number) => void;
}

const COLORS = {
  background: "#0e1116",
  grid: "#1d2430",
  axis: "#71809a",
  band: "#ffb30055",
  trace: "#ffb300",
  zero: "#3a8bff",
  gap: "#ff4d4d33",
  gapEdge: "#ff4d4d",
  selection: "#3a8bff22",
  selectionEdge: "#3a8bff",
  cursor: "#c8d3e6",
};

const PLOT_TOP = 12;
const AXIS_H = 26;
const LANE_H = 34;
const LEFT = 78;
const RIGHT = 12;

/** Nice round tick spacing at or below `target`, in the 1 / 2 / 5 sequence. */
function tickStep(target: number): number {
  const magnitude = Math.pow(10, Math.floor(Math.log10(target)));
  for (const multiple of [5, 2, 1]) {
    if (magnitude * multiple <= target) return magnitude * multiple;
  }
  return magnitude;
}

export function Timeline(props: Props) {
  const { view, full, buckets, marks, gaps, selection } = props;
  const canvasRef = useRef<HTMLCanvasElement | null>(null);
  const wrapRef = useRef<HTMLDivElement | null>(null);
  const [size, setSize] = useState({ w: 900, h: 340 });
  const [hover, setHover] = useState<{ x: number; y: number } | null>(null);
  const drag = useRef<{ mode: "select" | "pan"; x0: number; view0: View } | null>(null);

  // The plot geometry, shared by the renderer and by every hit test below. Deriving it twice
  // is how a click lands one pixel away from what it selected.
  const plotW = Math.max(1, size.w - LEFT - RIGHT);
  const plotH = Math.max(1, size.h - PLOT_TOP - AXIS_H - LANE_H);
  const spanNs = Math.max(1, view.end_ns - view.start_ns);

  const toX = useMemo(
    () => (t_ns: number) => LEFT + ((t_ns - view.start_ns) / spanNs) * plotW,
    [view.start_ns, spanNs, plotW],
  );
  const toTime = useMemo(
    () => (x: number) => view.start_ns + ((x - LEFT) / plotW) * spanNs,
    [view.start_ns, spanNs, plotW],
  );

  // Autoscale to what is on screen, not to the whole capture: zooming into the idle floor
  // should show the idle floor's structure, not a flat line at the bottom of a 90 mA axis.
  const [lo, hi] = useMemo(() => {
    let min = Infinity;
    let max = -Infinity;
    for (const b of buckets) {
      if (b.count === 0) continue;
      if (b.min_ua < min) min = b.min_ua;
      if (b.max_ua > max) max = b.max_ua;
    }
    if (!Number.isFinite(min)) return [0, 1000];
    if (max - min < 1) max = min + 1;
    const pad = (max - min) * 0.08;
    return [Math.min(0, min - pad), max + pad];
  }, [buckets]);

  const toY = useMemo(
    () => (ua: number) => PLOT_TOP + plotH - ((ua - lo) / (hi - lo)) * plotH,
    [lo, hi, plotH],
  );

  useEffect(() => {
    const element = wrapRef.current;
    if (!element) return;
    const observer = new ResizeObserver(() => {
      const rect = element.getBoundingClientRect();
      setSize({ w: Math.max(200, rect.width), h: Math.max(220, rect.height) });
    });
    observer.observe(element);
    return () => observer.disconnect();
  }, []);

  useEffect(() => props.onWidth(Math.round(plotW)), [plotW]);

  useEffect(() => {
    const canvas = canvasRef.current;
    if (!canvas) return;
    const dpr = window.devicePixelRatio || 1;
    canvas.width = Math.round(size.w * dpr);
    canvas.height = Math.round(size.h * dpr);
    const ctx = canvas.getContext("2d");
    if (!ctx) return;
    ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
    draw(ctx);
  });

  function draw(ctx: CanvasRenderingContext2D) {
    ctx.fillStyle = COLORS.background;
    ctx.fillRect(0, 0, size.w, size.h);

    drawGrid(ctx);
    drawGaps(ctx);
    drawTrace(ctx);
    drawLane(ctx);
    drawSelection(ctx);
    drawCursor(ctx);
  }

  function drawGrid(ctx: CanvasRenderingContext2D) {
    ctx.font = "11px ui-monospace, SFMono-Regular, Menlo, monospace";
    ctx.textBaseline = "middle";

    const currentStep = tickStep((hi - lo) / 5);
    ctx.strokeStyle = COLORS.grid;
    ctx.lineWidth = 1;
    for (let v = Math.ceil(lo / currentStep) * currentStep; v <= hi; v += currentStep) {
      const y = Math.round(toY(v)) + 0.5;
      ctx.beginPath();
      ctx.moveTo(LEFT, y);
      ctx.lineTo(LEFT + plotW, y);
      ctx.strokeStyle = v === 0 ? COLORS.zero : COLORS.grid;
      ctx.stroke();
      ctx.fillStyle = COLORS.axis;
      ctx.textAlign = "right";
      ctx.fillText(uA(v), LEFT - 8, y);
    }

    const timeStep = tickStep(spanNs / 6);
    ctx.textAlign = "center";
    ctx.textBaseline = "top";
    const axisY = PLOT_TOP + plotH + LANE_H;
    for (
      let t = Math.ceil(view.start_ns / timeStep) * timeStep;
      t <= view.end_ns;
      t += timeStep
    ) {
      const x = Math.round(toX(t)) + 0.5;
      ctx.strokeStyle = COLORS.grid;
      ctx.beginPath();
      ctx.moveTo(x, PLOT_TOP);
      ctx.lineTo(x, axisY);
      ctx.stroke();
      ctx.fillStyle = COLORS.axis;
      ctx.fillText(axisTime(t, spanNs), x, axisY + 6);
    }
  }

  /** Missing data is drawn as missing. Nothing else in this file matters more. */
  function drawGaps(ctx: CanvasRenderingContext2D) {
    for (const gap of gaps) {
      const x0 = toX(gap.start_ns);
      const x1 = toX(gap.end_ns);
      if (x1 < LEFT || x0 > LEFT + plotW) continue;
      const left = Math.max(LEFT, x0);
      const width = Math.max(2, Math.min(LEFT + plotW, x1) - left);
      ctx.fillStyle = COLORS.gap;
      ctx.fillRect(left, PLOT_TOP, width, plotH);
      ctx.strokeStyle = COLORS.gapEdge;
      ctx.setLineDash([3, 3]);
      ctx.strokeRect(left + 0.5, PLOT_TOP + 0.5, width - 1, plotH - 1);
      ctx.setLineDash([]);
    }
  }

  function drawTrace(ctx: CanvasRenderingContext2D) {
    if (buckets.length === 0) return;

    // The min/max band first, then the mean over it: the band is the honest extent of what
    // was measured in that pixel column, the line is where it spent its time.
    ctx.fillStyle = COLORS.band;
    for (const b of buckets) {
      if (b.count === 0) continue;
      const x = toX(b.t_ns);
      const top = toY(b.max_ua);
      const height = Math.max(1, toY(b.min_ua) - top);
      ctx.fillRect(x, top, Math.max(1, plotW / buckets.length + 0.5), height);
    }

    ctx.strokeStyle = COLORS.trace;
    ctx.lineWidth = 1.25;
    ctx.beginPath();
    let open = false;
    for (const b of buckets) {
      if (b.count === 0) {
        open = false; // A break in the path, so the eye never connects across nothing.
        continue;
      }
      const x = toX(b.t_ns);
      const y = toY(b.mean_ua);
      if (open) ctx.lineTo(x, y);
      else ctx.moveTo(x, y);
      open = true;
    }
    ctx.stroke();
  }

  /** The firmware-event lane under the plot. */
  function drawLane(ctx: CanvasRenderingContext2D) {
    const top = PLOT_TOP + plotH;
    ctx.fillStyle = "#11161f";
    ctx.fillRect(LEFT, top, plotW, LANE_H);

    ctx.font = "10px ui-monospace, SFMono-Regular, Menlo, monospace";
    ctx.textBaseline = "middle";
    ctx.textAlign = "left";

    let lastLabelX = -Infinity;
    let lastLabel = "";
    for (const mark of marks) {
      const x = toX(mark.t_ns);
      if (x < LEFT - 1 || x > LEFT + plotW + 1) continue;
      // A stop is drawn shorter and dimmer than its start: the pair reads as a bracket, and
      // the lane does not claim two events happened where one did.
      ctx.strokeStyle = eventColor(mark.name);
      ctx.globalAlpha = mark.is_stop ? 0.45 : 1;
      ctx.lineWidth = 1;
      ctx.beginPath();
      ctx.moveTo(Math.round(x) + 0.5, top + (mark.is_stop ? 12 : 4));
      ctx.lineTo(Math.round(x) + 0.5, top + LANE_H - 4);
      ctx.stroke();
      ctx.globalAlpha = 1;

      // Label a name once per run, and only where one fits. Repeating "SENSOR_READ" every
      // 70 pixels buries the rarer event that someone actually opened the capture to find.
      const room = mark.name === lastLabel ? 260 : 80;
      if (!mark.is_stop && x - lastLabelX > room) {
        ctx.fillStyle = eventColor(mark.name);
        ctx.fillText(mark.name, x + 3, top + LANE_H / 2);
        lastLabelX = x;
        lastLabel = mark.name;
      }
    }
  }

  function drawSelection(ctx: CanvasRenderingContext2D) {
    if (!selection) return;
    const x0 = Math.max(LEFT, toX(selection.start_ns));
    const x1 = Math.min(LEFT + plotW, toX(selection.end_ns));
    if (x1 <= x0) return;
    ctx.fillStyle = COLORS.selection;
    ctx.fillRect(x0, PLOT_TOP, x1 - x0, plotH + LANE_H);
    ctx.strokeStyle = COLORS.selectionEdge;
    ctx.beginPath();
    ctx.moveTo(Math.round(x0) + 0.5, PLOT_TOP);
    ctx.lineTo(Math.round(x0) + 0.5, PLOT_TOP + plotH + LANE_H);
    ctx.moveTo(Math.round(x1) + 0.5, PLOT_TOP);
    ctx.lineTo(Math.round(x1) + 0.5, PLOT_TOP + plotH + LANE_H);
    ctx.stroke();
  }

  function drawCursor(ctx: CanvasRenderingContext2D) {
    if (!hover || hover.x < LEFT || hover.x > LEFT + plotW) return;
    const x = Math.round(hover.x) + 0.5;
    ctx.strokeStyle = COLORS.cursor;
    ctx.globalAlpha = 0.4;
    ctx.beginPath();
    ctx.moveTo(x, PLOT_TOP);
    ctx.lineTo(x, PLOT_TOP + plotH + LANE_H);
    ctx.stroke();
    ctx.globalAlpha = 1;

    const index = Math.min(
      buckets.length - 1,
      Math.max(0, Math.floor(((hover.x - LEFT) / plotW) * buckets.length)),
    );
    const bucket = buckets[index];
    const time = axisTime(toTime(hover.x), spanNs);
    const label =
      bucket && bucket.count > 0
        ? `${time}   ${uA(bucket.mean_ua)}  (${uA(bucket.min_ua)} .. ${uA(bucket.max_ua)})`
        : `${time}   no samples`;

    ctx.font = "11px ui-monospace, SFMono-Regular, Menlo, monospace";
    const width = ctx.measureText(label).width + 12;
    const boxX = Math.min(LEFT + plotW - width, hover.x + 8);
    ctx.fillStyle = "#1b2432ee";
    ctx.fillRect(boxX, PLOT_TOP + 4, width, 20);
    ctx.fillStyle = "#e6ecf5";
    ctx.textAlign = "left";
    ctx.textBaseline = "middle";
    ctx.fillText(label, boxX + 6, PLOT_TOP + 14);
  }

  // ----------------------------------------------------------------- input

  function localX(e: React.MouseEvent): number {
    const rect = (e.currentTarget as HTMLElement).getBoundingClientRect();
    return e.clientX - rect.left;
  }

  function clampView(v: View): View {
    const width = Math.min(v.end_ns - v.start_ns, full.end_ns - full.start_ns);
    let start = Math.max(full.start_ns, Math.min(v.start_ns, full.end_ns - width));
    return { start_ns: start, end_ns: start + width };
  }

  // A native listener rather than React's onWheel, because React registers wheel handlers
  // passively and a passive handler cannot preventDefault — the zoom would work and scroll
  // the window underneath it at the same time.
  useEffect(() => {
    const canvas = canvasRef.current;
    if (!canvas) return;
    canvas.addEventListener("wheel", onWheel, { passive: false });
    return () => canvas.removeEventListener("wheel", onWheel);
  });

  function onWheel(e: WheelEvent) {
    const rect = (e.currentTarget as HTMLElement).getBoundingClientRect();
    const x = e.clientX - rect.left;
    if (x < LEFT) return;
    e.preventDefault();
    const anchor = toTime(x);
    const factor = Math.exp(e.deltaY * 0.0015);
    // One microsecond is already finer than any sample period this tool captures; zooming
    // past it would only show interpolation.
    const width = Math.max(1000, Math.min(full.end_ns - full.start_ns, spanNs * factor));
    const ratio = (anchor - view.start_ns) / spanNs;
    props.onView(clampView({ start_ns: anchor - ratio * width, end_ns: anchor - ratio * width + width }));
  }

  function onMouseDown(e: React.MouseEvent) {
    const x = localX(e);
    if (x < LEFT) return;
    const mode = e.shiftKey || e.button === 1 ? "pan" : "select";
    drag.current = { mode, x0: x, view0: view };
    if (mode === "select") props.onSelection({ start_ns: toTime(x), end_ns: toTime(x) });
  }

  function onMouseMove(e: React.MouseEvent) {
    const x = localX(e);
    const rect = (e.currentTarget as HTMLElement).getBoundingClientRect();
    setHover({ x, y: e.clientY - rect.top });

    const state = drag.current;
    if (!state) return;
    if (state.mode === "pan") {
      const shift = ((state.x0 - x) / plotW) * (state.view0.end_ns - state.view0.start_ns);
      props.onView(
        clampView({ start_ns: state.view0.start_ns + shift, end_ns: state.view0.end_ns + shift }),
      );
    } else {
      const a = toTime(state.x0);
      const b = toTime(x);
      props.onSelection({ start_ns: Math.min(a, b), end_ns: Math.max(a, b) });
    }
  }

  function onMouseUp(e: React.MouseEvent) {
    const state = drag.current;
    drag.current = null;
    // A click with no drag clears the selection instead of leaving a zero-width one, which
    // would ask the core to integrate over nothing.
    if (state?.mode === "select" && Math.abs(localX(e) - state.x0) < 3) props.onSelection(null);
  }

  return (
    <div className="timeline" ref={wrapRef}>
      <canvas
        ref={canvasRef}
        style={{ width: size.w, height: size.h }}
        onMouseDown={onMouseDown}
        onMouseMove={onMouseMove}
        onMouseUp={onMouseUp}
        onMouseLeave={() => {
          setHover(null);
          drag.current = null;
        }}
        onDoubleClick={() => {
          props.onView(full);
          props.onSelection(null);
        }}
      />
    </div>
  );
}
