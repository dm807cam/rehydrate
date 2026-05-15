import { useEffect, useState } from "react";
import { ipc, onDeviceReachable } from "../ipc";
import { Icon } from "./Icon";
import type { DeviceState } from "../types";

interface Props {
  defaultPath: string | null;
  initialDevice: DeviceState | null;
  onOpenLibrary: () => void;
  onSkip: () => void;
}

/* Two-step first-run flow: plug in the tablet, then open a library.
 * The third "pair tablet" step that used to live here was unreachable
 * — the moment the user opened a library, the host unmounted this
 * component. Pairing is surfaced separately by the StatusPill in the
 * toolbar (the connect/password flow is one click from there), so we
 * don't need to bake it into onboarding. */
export function Onboarding({
  defaultPath,
  initialDevice,
  onOpenLibrary,
  onSkip,
}: Props) {
  const [step, setStep] = useState(0);
  const [device, setDevice] = useState<DeviceState | null>(initialDevice);

  useEffect(() => {
    // Issue #37: the onboarding flow can dismiss before the listener
    // attach promise resolves (user clicks "Skip" fast, or the
    // reachability watcher fires the auto-advance below before the
    // listener was installed). Without `cancelled` the listener
    // stays attached forever and `setDevice` runs against an
    // unmounted component on every subsequent reachability event.
    let cancelled = false;
    let unlisten: (() => void) | undefined;
    onDeviceReachable(() => {
      ipc.deviceState().then(setDevice).catch(() => {});
    }).then((u) => {
      if (cancelled) u();
      else unlisten = u;
    });
    return () => {
      cancelled = true;
      unlisten?.();
    };
  }, []);

  // Auto-advance from "plug in tablet" to "open library" once the
  // tablet is detected. The user gets visible feedback that the app
  // saw their plug-in.
  useEffect(() => {
    if (step === 0 && device?.reachable) {
      const t = setTimeout(() => setStep(1), 500);
      return () => clearTimeout(t);
    }
  }, [step, device?.reachable]);

  if (step === 0) {
    return (
      <div className="onboarding">
        <div className="step-art">
          <Icon name="plug" size={56} />
        </div>
        <h1>Plug in your reMarkable</h1>
        <p>
          Connect the tablet to this Mac with its USB-C cable. reHydrate
          watches for it automatically — you don't need to do anything else.
        </p>
        <div className="actions">
          <button onClick={onSkip}>Skip for now</button>
          <button className="primary" onClick={() => setStep(1)}>
            I'll plug it in later
          </button>
        </div>
        <Dots step={step} />
        <p className="muted small">
          {device?.reachable
            ? "Tablet detected — moving on…"
            : "Waiting for tablet…"}
        </p>
      </div>
    );
  }

  return (
    <div className="onboarding">
      <div className="step-art">
        <Icon name="library" size={56} />
      </div>
      <h1>Open a library</h1>
      <p>
        Your library is a single folder on this machine that holds every
        version of every document. No cloud, no telemetry.
      </p>
      {defaultPath && (
        <p className="muted small">
          Default location: <code>{defaultPath}</code>
        </p>
      )}
      <div className="actions">
        <button onClick={() => setStep(0)}>Back</button>
        <button
          className="primary"
          onClick={onOpenLibrary}
          disabled={!defaultPath}
        >
          <Icon name="library" /> Open default library
        </button>
      </div>
      <Dots step={step} />
    </div>
  );
}

function Dots({ step }: { step: number }) {
  return (
    <div className="dots">
      {[0, 1].map((i) => (
        <span key={i} className={`dot${i === step ? " active" : ""}`} />
      ))}
    </div>
  );
}
