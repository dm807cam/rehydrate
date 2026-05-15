// Device connection state + the three actions that mutate it,
// extracted from App.tsx.
//
// `device: DeviceState` carries reachability + connected + stored-
// password flags + the cached `DeviceInfo` block. The hook keeps it
// in sync with three sources:
//
//   - The Tauri `device:reachable` event (`onDeviceReachable`). Fires
//     whenever the OS sees the USB-ethernet endpoint at 10.11.99.1
//     appear or disappear; we refetch the full state to pick up
//     side effects (e.g. an auto-cleared `connected` after unplug).
//   - The explicit `tryConnect` / `disconnect` / `submitPassword`
//     actions wired to the StatusPill / PasswordDialog.
//   - Whatever the parent component does — e.g. the initial-load
//     `deviceState()` fetch in App.tsx still uses `setDevice`.
//
// The hook deliberately does NOT own the initial-load fetch; App
// batches that with the library + recent-libraries hydrate so all
// three land before the first render commits.

import { useCallback, useEffect, useRef, useState } from "react";

import { humanizeSyncError } from "../humanizeError";
import { ipc, onDeviceReachable } from "../ipc";
import type { DeviceState } from "../types";

export interface UseDeviceSyncInjections {
  /// App-level error sink. `tryConnect` routes failures through it.
  setError: (msg: string | null) => void;
  /// Triggers the PasswordDialog when the user clicks Connect and we
  /// have no stored password yet.
  openPasswordDialog: () => void;
  /// Lets the password-dialog success path dismiss itself.
  closePasswordDialog: () => void;
}

export interface UseDeviceSyncResult {
  device: DeviceState | null;
  setDevice: React.Dispatch<React.SetStateAction<DeviceState | null>>;
  tryConnect: () => Promise<void>;
  disconnect: () => Promise<void>;
  submitPassword: (password: string, remember: boolean) => Promise<void>;
}

export function useDeviceSync(
  injections: UseDeviceSyncInjections,
): UseDeviceSyncResult {
  // Stash injections in a ref so callbacks below don't have to
  // re-memoize when the App passes inline arrows that change
  // identity every render. See `useLibrary` for the same pattern.
  const injectionsRef = useRef(injections);
  injectionsRef.current = injections;
  const [device, setDevice] = useState<DeviceState | null>(null);

  // Single subscription to the Tauri-emitted reachability event.
  // The payload itself isn't strictly necessary — we just refetch
  // the full `DeviceState` because the event also flips connected /
  // disconnected as a side effect of the watcher.
  useEffect(() => {
    // Issue #37: if the parent unmounts this hook before the
    // listener attach promise resolves, `unlisten` is still
    // undefined when cleanup runs — the listener stays attached
    // forever and the closure holds setDevice pointing at unmounted
    // state. Track `cancelled` so the resolver either installs the
    // unlisten or invokes it immediately if cleanup beat it.
    let cancelled = false;
    let unlisten: (() => void) | undefined;
    onDeviceReachable(() => {
      ipc
        .deviceState()
        .then(setDevice)
        .catch(() => {
          /* deviceState is best-effort; a transient failure is fine */
        });
    }).then((u) => {
      if (cancelled) u();
      else unlisten = u;
    });
    return () => {
      cancelled = true;
      unlisten?.();
    };
  }, []);

  const tryConnect = useCallback(async () => {
    const { setError, openPasswordDialog } = injectionsRef.current;
    setError(null);
    try {
      if (device?.has_stored_password) {
        await ipc.connectDevice();
        setDevice(await ipc.deviceState());
      } else {
        openPasswordDialog();
      }
    } catch (e) {
      injectionsRef.current.setError(humanizeSyncError(e));
    }
  }, [device?.has_stored_password]);

  const disconnect = useCallback(async () => {
    await ipc.disconnectDevice();
    setDevice(await ipc.deviceState());
  }, []);

  const submitPassword = useCallback(
    async (password: string, remember: boolean) => {
      await ipc.connectDevice(password, remember);
      setDevice(await ipc.deviceState());
      injectionsRef.current.closePasswordDialog();
    },
    [],
  );

  return { device, setDevice, tryConnect, disconnect, submitPassword };
}
