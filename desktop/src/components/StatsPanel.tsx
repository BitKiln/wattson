// The numbers for whatever is selected.
//
// Every value here comes back from `region_stats`. The panel formats and nothing else: if a
// figure shown here were computed in TypeScript, the window and `wattson analyze` could
// disagree about the same capture, and only one of them would be in the test suite.

import type { RegionStats } from "../api";
import { seconds, uA, uC, uJ, uV, uW } from "../format";

export function StatsPanel({ stats }: { stats: RegionStats | null }) {
  if (!stats) {
    return (
      <section className="panel">
        <h2>Selection</h2>
        <p className="hint">
          Drag across the plot to select a span. Shift-drag pans, the wheel zooms, and a
          double-click returns to the whole capture.
        </p>
      </section>
    );
  }

  const rows: [string, string][] = [
    ["duration", seconds(stats.duration_s)],
    ["samples", stats.sample_count.toLocaleString()],
    ["energy", uJ(stats.energy_uj)],
    ["charge", `${uC(stats.charge_uc)}  (${stats.charge_mah.toPrecision(3)} mAh)`],
    ["mean current", uA(stats.current_mean_ua)],
    ["RMS current", uA(stats.current_rms_ua)],
    ["min / max current", `${uA(stats.current_min_ua)} / ${uA(stats.current_max_ua)}`],
    ["mean voltage", uV(stats.voltage_mean_uv)],
    ["mean power", uW(stats.power_mean_uw)],
    ["peak power", uW(stats.power_peak_uw)],
  ];

  return (
    <section className="panel">
      <h2>Selection</h2>
      <table className="kv">
        <tbody>
          {rows.map(([key, value]) => (
            <tr key={key}>
              <th>{key}</th>
              <td>{value}</td>
            </tr>
          ))}
        </tbody>
      </table>
    </section>
  );
}
