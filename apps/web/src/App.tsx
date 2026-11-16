import type { EngineInfo } from "@reticula/protocol";
import { useEffect, useState } from "react";
import { startEngine } from "./engine/client.ts";

type EngineState =
  | { status: "starting" }
  | { status: "ready"; info: EngineInfo }
  | { status: "failed"; message: string };

export function App() {
  const [engine, setEngine] = useState<EngineState>({ status: "starting" });

  useEffect(
    () =>
      startEngine((message) => {
        switch (message.kind) {
          case "ready":
            setEngine({ status: "ready", info: message.info });
            break;
          case "failed":
            setEngine({ status: "failed", message: message.message });
            break;
        }
      }),
    [],
  );

  return (
    <main>
      <h1>Reticula</h1>
      <dl>
        <dt>Engine</dt>
        <dd data-testid="engine-status">{engine.status}</dd>
        {engine.status === "ready" && (
          <>
            <dt>Kernel</dt>
            <dd data-testid="kernel-version">{engine.info.kernelVersion}</dd>
            <dt>Determinism digest</dt>
            <dd data-testid="determinism-digest">{engine.info.determinismDigest}</dd>
          </>
        )}
        {engine.status === "failed" && (
          <>
            <dt>Error</dt>
            <dd data-testid="engine-error">{engine.message}</dd>
          </>
        )}
      </dl>
    </main>
  );
}
