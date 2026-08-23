// Demo app for @openmedvideo/player-react. Build + serve via ../serve.sh
// or: npx esbuild app.jsx --bundle --outfile=bundle.js
import { createRoot } from "react-dom/client";
import { useRef, useState } from "react";
import { OmvPlayer } from "@openmedvideo/player-react";

function App() {
  const q = new URLSearchParams(location.search);
  const [status, setStatus] = useState("loading…");
  const [frame, setFrame] = useState(null);
  const player = useRef(null);

  return (
    <div>
      <h1>@openmedvideo/player-react demo</h1>
      <p id="status">{status}{frame && ` · frame ${frame.frame}/${frame.frames}`}</p>
      <button id="step" onClick={() => player.current?.step(1)}>step +1 (via ref)</button>
      <div style={{ height: 560, marginTop: 8 }}>
        <OmvPlayer
          ref={player}
          server={q.get("server") || "http://localhost:8000"}
          studyId={q.get("study") || ""}
          token={q.get("token") || ""}
          style={{ height: "100%" }}
          onReady={(e) => setStatus(`ready: ${e.studyUid.slice(-12)}`)}
          onError={(e) => setStatus(`error: ${e.message}`)}
          onFrame={setFrame}
        />
      </div>
    </div>
  );
}

createRoot(document.getElementById("root")).render(<App />);
