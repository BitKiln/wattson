// Display formatting.
//
// Formatting is presentation, so it lives here rather than in `wattson-core` — but the rule
// it follows is the same one the CLI follows: never round a number into a different answer.
// Three significant figures is enough to read and not enough to mislead, and the unit is
// always shown.

const SI = [
  { limit: 1e9, suffix: "G" },
  { limit: 1e6, suffix: "M" },
  { limit: 1e3, suffix: "k" },
  { limit: 1, suffix: "" },
  { limit: 1e-3, suffix: "m" },
  { limit: 1e-6, suffix: "u" },
  { limit: 1e-9, suffix: "n" },
  { limit: 1e-12, suffix: "p" },
];

/** `0.00095` seconds becomes `950 us`. A zero keeps its unit rather than becoming bare `0`. */
export function si(value: number, unit: string, digits = 3): string {
  if (!Number.isFinite(value)) return `- ${unit}`;
  if (value === 0) return `0 ${unit}`;
  const magnitude = Math.abs(value);
  const step = SI.find((s) => magnitude >= s.limit) ?? SI[SI.length - 1];
  const scaled = value / step.limit;
  return `${trim(scaled, digits)} ${step.suffix}${unit}`;
}

function trim(value: number, digits: number): string {
  const text = value.toPrecision(digits);
  return text.includes(".") ? text.replace(/\.?0+$/, "") : text;
}

/** Microamps as the core reports them, displayed as amps with an SI prefix. */
export const uA = (v: number) => si(v * 1e-6, "A");
export const uV = (v: number) => si(v * 1e-6, "V");
export const uW = (v: number) => si(v * 1e-6, "W");
export const uJ = (v: number) => si(v * 1e-6, "J");
export const uC = (v: number) => si(v * 1e-6, "C");
export const us = (v: number) => si(v * 1e-6, "s");
export const ns = (v: number) => si(v * 1e-9, "s");
export const seconds = (v: number) => si(v, "s");

/** Axis labels want a consistent unit across the whole axis, not one per tick. */
export function axisTime(t_ns: number, span_ns: number): string {
  if (span_ns >= 1e9) return `${(t_ns / 1e9).toFixed(span_ns >= 1e10 ? 1 : 3)} s`;
  if (span_ns >= 1e6) return `${(t_ns / 1e6).toFixed(span_ns >= 1e7 ? 1 : 3)} ms`;
  if (span_ns >= 1e3) return `${(t_ns / 1e3).toFixed(span_ns >= 1e4 ? 1 : 3)} us`;
  return `${t_ns.toFixed(0)} ns`;
}

export function percent(ratio: number): string {
  return `${(ratio * 100).toPrecision(3)} %`;
}

/** A stable colour per event name, so a name keeps its colour between captures. */
export function eventColor(name: string): string {
  let hash = 0;
  for (let i = 0; i < name.length; i++) {
    hash = (hash * 31 + name.charCodeAt(i)) >>> 0;
  }
  // Golden-angle steps, so two names that hash close together still land far apart on the
  // wheel. The colour has to stay stable across captures, which is why it is derived from the
  // name rather than assigned by position in a list.
  return `hsl(${(hash * 137.508) % 360} 72% 62%)`;
}
