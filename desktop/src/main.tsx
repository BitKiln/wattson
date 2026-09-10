// Scaffold: phase 2. The window content is not built yet.
//
// The plan it stands in for: three vertically stacked tracks — current, voltage, and firmware
// events — sharing one time axis, where zooming or panning any of them moves all of them.
// That shared axis is the product; everything else is a chart.

import React from "react";
import ReactDOM from "react-dom/client";

function App() {
  return (
    <main style={{ fontFamily: "system-ui", padding: "2rem", lineHeight: 1.5 }}>
      <h1>Wattson</h1>
      <p>
        Desktop scaffold. The analysis engine is complete and reachable through the Tauri
        commands in <code>src-tauri/src/commands.rs</code>; the timeline UI is phase 2.
      </p>
      <p>
        Everything the UI will show already works from the command line:
        <code> wattson analyze capture.pprof --events --plot</code>
      </p>
    </main>
  );
}

ReactDOM.createRoot(document.getElementById("root")!).render(
  <React.StrictMode>
    <App />
  </React.StrictMode>,
);
