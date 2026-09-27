// The window.
//
// Everything on screen is a rendering of something `wattson-core` returned. The state here is
// only about what the viewer is looking at — which file, which time range, which selection —
// and never about what a measurement means.

import { useCallback, useEffect, useRef, useState } from "react";
import { open } from "@tauri-apps/plugin-dialog";

import { api, errorMessage } from "./api";
import type { CaptureInfo, EventMark, EventStats, RegionStats, Bucket } from "./api";
import { CaptureBar } from "./components/CaptureBar";
import { EventTable } from "./components/EventTable";
import { StatsPanel } from "./components/StatsPanel";
import { Timeline } from "./components/Timeline";
import type { Selection, View } from "./components/Timeline";

const EMPTY_VIEW: View = { start_ns: 0, end_ns: 1 };

export function App() {
  const [info, setInfo] = useState<CaptureInfo | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const [full, setFull] = useState<View>(EMPTY_VIEW);
  const [view, setView] = useState<View>(EMPTY_VIEW);
  const [width, setWidth] = useState(900);
  const [buckets, setBuckets] = useState<Bucket[]>([]);
  const [marks, setMarks] = useState<EventMark[]>([]);
  const [events, setEvents] = useState<EventStats[]>([]);
  const [selection, setSelection] = useState<Selection | null>(null);
  const [regionStats, setRegionStats] = useState<RegionStats | null>(null);

  // Requests are answered out of order when one zoom follows another quickly. Only the newest
  // is allowed to land, otherwise the plot briefly shows the previous zoom level's data.
  const waveformRequest = useRef(0);
  const selectionRequest = useRef(0);

  const openCapture = useCallback(async () => {
    const chosen = await open({
      multiple: false,
      filters: [{ name: "Wattson capture", extensions: ["pprof"] }],
    });
    if (typeof chosen !== "string") return;

    setBusy(true);
    setError(null);
    try {
      const opened = await api.openCapture(chosen);
      const span: View = {
        start_ns: 0,
        end_ns: Math.max(1, Math.round(opened.duration_s * 1e9)),
      };
      setInfo(opened);
      setFull(span);
      setView(span);
      setSelection(null);
      setRegionStats(null);
      setMarks(await api.eventMarks());
      setEvents(await api.eventTable());
    } catch (e) {
      setError(errorMessage(e));
      setInfo(null);
      setBuckets([]);
      setMarks([]);
      setEvents([]);
    } finally {
      setBusy(false);
    }
  }, []);

  useEffect(() => {
    if (!info) return;
    const ticket = ++waveformRequest.current;
    api
      .waveform(Math.round(view.start_ns), Math.round(view.end_ns), Math.max(1, width))
      .then((b) => {
        if (ticket === waveformRequest.current) setBuckets(b);
      })
      .catch((e) => setError(errorMessage(e)));
  }, [info, view.start_ns, view.end_ns, width]);

  useEffect(() => {
    if (!info || !selection || selection.end_ns - selection.start_ns < 1000) {
      setRegionStats(null);
      return;
    }
    const ticket = ++selectionRequest.current;
    api
      .selectionStats(Math.round(selection.start_ns), Math.round(selection.end_ns))
      .then((s) => {
        if (ticket === selectionRequest.current) setRegionStats(s);
      })
      .catch((e) => setError(errorMessage(e)));
  }, [info, selection?.start_ns, selection?.end_ns]);

  /** Clicking an event row puts the first occurrence of it on screen. */
  const focusEvent = useCallback(
    (name: string) => {
      const first = marks.find((m) => m.name === name);
      if (!first) return;
      const width_ns = Math.max(10_000, (full.end_ns - full.start_ns) / 200);
      const start = Math.max(full.start_ns, first.t_ns - width_ns / 2);
      setView({ start_ns: start, end_ns: Math.min(full.end_ns, start + width_ns) });
    },
    [marks, full],
  );

  return (
    <div className="app">
      <CaptureBar info={info} onOpen={openCapture} busy={busy} />

      {error && (
        <div className="error" onClick={() => setError(null)}>
          {error}
        </div>
      )}

      {info ? (
        <>
          <Timeline
            view={view}
            full={full}
            buckets={buckets}
            marks={marks}
            gaps={info.gaps}
            selection={selection}
            onView={setView}
            onSelection={setSelection}
            onWidth={setWidth}
          />
          <div className="lower">
            <StatsPanel stats={regionStats} />
            <EventTable events={events} onPick={focusEvent} />
          </div>
        </>
      ) : (
        <Welcome onOpen={openCapture} />
      )}
    </div>
  );
}

function Welcome({ onOpen }: { onOpen: () => void }) {
  return (
    <main className="welcome">
      <h1>Open a capture</h1>
      <p>
        A <code>.pprof</code> file records what the current was doing and what the firmware
        said it was doing, on one clock. This window puts them on one axis.
      </p>
      <p className="hint">
        No hardware yet? The simulator produces a real capture:
        <br />
        <code>wattson sim --listen 127.0.0.1:9000 --profile ble-sensor</code>
        <br />
        <code>
          wattson capture --device tcp://127.0.0.1:9000 --duration 10s --metadata events.toml
          --out cap.pprof
        </code>
      </p>
      <button onClick={onOpen}>Open capture</button>
    </main>
  );
}
