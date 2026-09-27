// Per-event energy: the table that makes this a firmware profiler rather than a current
// monitor.
//
// The warning column is not decoration. An event with unterminated starts, orphaned stops, or
// occurrences too short to sample has statistics computed from a subset of what happened, and
// a reader who cannot see that will quote the mean anyway.

import { useState } from "react";
import type { EventStats } from "../api";
import { eventColor, percent, uA, uJ, us } from "../format";

type SortKey = "name" | "occurrences" | "mean_energy" | "p95_energy" | "total_energy";

export function EventTable({
  events,
  onPick,
}: {
  events: EventStats[];
  onPick: (name: string) => void;
}) {
  const [sort, setSort] = useState<SortKey>("total_energy");

  const sorted = [...events].sort((a, b) => {
    switch (sort) {
      case "name":
        return a.name.localeCompare(b.name);
      case "occurrences":
        return b.occurrences - a.occurrences;
      case "mean_energy":
        return b.energy_uj.mean - a.energy_uj.mean;
      case "p95_energy":
        return b.energy_uj.p95 - a.energy_uj.p95;
      default:
        return b.total_energy_uj - a.total_energy_uj;
    }
  });

  if (events.length === 0) {
    return (
      <section className="panel">
        <h2>Events</h2>
        <p className="hint">
          This capture carries no named events. Names come from the metadata file passed to
          <code> wattson capture --metadata</code>; the ids on the wire are opaque by design.
        </p>
      </section>
    );
  }

  const header = (key: SortKey, label: string) => (
    <th
      className={`sortable${sort === key ? " active" : ""}`}
      onClick={() => setSort(key)}
      title={`Sort by ${label}`}
    >
      {label}
    </th>
  );

  return (
    <section className="panel grow">
      <h2>Events</h2>
      <div className="table-scroll">
        <table className="events">
          <thead>
            <tr>
              {header("name", "event")}
              {header("occurrences", "n")}
              <th>duration (mean)</th>
              {header("mean_energy", "energy (mean)")}
              {header("p95_energy", "energy (P95)")}
              {header("total_energy", "energy (total)")}
              <th>mean / peak current</th>
              <th>duty</th>
              <th>warnings</th>
            </tr>
          </thead>
          <tbody>
            {sorted.map((e) => (
              <tr key={e.name} onClick={() => onPick(e.name)}>
                <td>
                  <span className="swatch" style={{ background: eventColor(e.name) }} />
                  {e.name}
                </td>
                <td className="num">{e.occurrences}</td>
                <td className="num">{us(e.duration_us.mean)}</td>
                <td className="num">{uJ(e.energy_uj.mean)}</td>
                <td className="num">{uJ(e.energy_uj.p95)}</td>
                <td className="num">{uJ(e.total_energy_uj)}</td>
                <td className="num">
                  {uA(e.current_mean_ua)} / {uA(e.current_peak_ua)}
                </td>
                <td className="num">{percent(e.duty_cycle)}</td>
                <td className="warn">{warnings(e)}</td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
    </section>
  );
}

function warnings(e: EventStats): string {
  const notes: string[] = [];
  if (e.unterminated > 0) notes.push(`${e.unterminated} never stopped`);
  if (e.orphaned_stops > 0) notes.push(`${e.orphaned_stops} stops without a start`);
  if (e.unsampled > 0) notes.push(`${e.unsampled} too short to sample`);
  return notes.join(", ");
}
