// What this capture is, and every reason not to trust it.
//
// The integrity line is deliberately loud. A truncated or gap-ridden capture still produces
// perfectly plausible energy numbers — lower ones — and that is the failure mode this whole
// project is built to refuse.

import type { CaptureInfo } from "../api";
import { GAP_CAUSE_TEXT } from "../api";
import { ns, seconds, si } from "../format";

export function CaptureBar({
  info,
  onOpen,
  busy,
}: {
  info: CaptureInfo | null;
  onOpen: () => void;
  busy: boolean;
}) {
  return (
    <header className="bar">
      <div className="brand">
        <span className="logo" aria-hidden="true" />
        <strong>Wattson</strong>
      </div>

      {info ? (
        <div className="facts">
          <span title={info.path}>{fileName(info.path)}</span>
          <span>{seconds(info.duration_s)}</span>
          <span>{info.sample_count.toLocaleString()} samples</span>
          <span title="Derived from the data, not from what the capture was configured for">
            {si(info.effective_rate_hz, "sps")}
          </span>
          <span>{info.event_count.toLocaleString()} events</span>
          <span className="dim">
            {info.device_serial || "unknown device"} · fw {info.firmware_version || "?"} ·{" "}
            {info.compression}
          </span>
        </div>
      ) : (
        <div className="facts dim">No capture open</div>
      )}

      <button onClick={onOpen} disabled={busy}>
        {busy ? "Opening..." : "Open capture"}
      </button>

      {info && <Integrity info={info} />}
    </header>
  );
}

function Integrity({ info }: { info: CaptureInfo }) {
  const problems: string[] = [];
  if (!info.finalized) problems.push("not finalized: the writer never closed this file");
  if (info.recovered) problems.push("recovered by forward scan after truncation");
  if (info.corrupt_chunks > 0) problems.push(`${info.corrupt_chunks} corrupt chunks skipped`);
  for (const gap of info.gaps) {
    problems.push(
      `gap of ${ns(gap.end_ns - gap.start_ns)} at ${ns(gap.start_ns)} (${GAP_CAUSE_TEXT[gap.cause] ?? gap.cause})`,
    );
  }

  if (problems.length === 0) {
    return <div className="integrity ok">complete</div>;
  }
  return (
    <div className="integrity bad" title={problems.join("\n")}>
      {problems.length} integrity {problems.length === 1 ? "problem" : "problems"}
      <ul>
        {problems.slice(0, 4).map((p) => (
          <li key={p}>{p}</li>
        ))}
        {problems.length > 4 && <li>and {problems.length - 4} more</li>}
      </ul>
    </div>
  );
}

function fileName(path: string): string {
  const parts = path.split(/[\\/]/);
  return parts[parts.length - 1] || path;
}
