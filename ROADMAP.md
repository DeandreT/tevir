# Roadmap

## 0. Foundation

- Typed node identities, geometry, physical input events, and validated screen
  topology.
- Exact-version handshake/session messages with bounded framing and input
  batches.
- Strict controller/agent configuration and native environment diagnostics.
- Windows and Linux CI coverage.

## 1. Native Input

- [x] Implement a Linux Wayland backend with XDG InputCapture and RemoteDesktop
  portals plus EIS event transport.
- [x] Implement a Windows backend with a dedicated message loop, low-level input
  hooks, and `SendInput`.
- [x] Normalize physical keys, high-resolution scrolling, button state, and display
  geometry into `domain` types.
- [x] Guarantee that focus loss, disconnect, and backend failure release every held
  key and button.
- [x] Add backend contract tests. Graphical smoke coverage is part of the
  desktop UI harness because portal sessions require an application window.

## 2. Secure Transport

- [x] Add persistent node identities and an explicit local pairing flow.
- [x] Establish mutually authenticated, encrypted sessions; plaintext input
  transport will not be supported.
- [x] Separate control and bulk clipboard traffic so clipboard transfer cannot
  delay key or button events.
- [x] Bound handshakes, queues, frame sizes, timeouts, and reconnect behavior.
- [x] Add malformed-frame, untrusted-peer, replay, and connection-loss tests.

## 3. Routing Session

- Build the controller and agent state machines around focus epochs and
  acknowledged event sequences.
- Route edge crossings through `Topology`, including mixed resolutions and
  negative coordinates.
- Coalesce pointer motion without coalescing key or button transitions.
- Reconcile display changes and prevent stale input after focus moves.

## 4. Clipboard

- Begin with bounded UTF-8 text clipboard transfer.
- Add ownership generations to prevent clipboard feedback loops.
- Keep platform-native clipboard formats out of the protocol.
- Evaluate additional MIME types independently after text transfer is hardened.

## 5. Desktop Product

- Add a native configuration and pairing UI backed by the same validated
  application commands as the CLI.
- Add tray status, reconnect controls, logs, and actionable permission errors.
- Package user-session startup for Windows and Linux Wayland.
- Add upgrade, recovery, soak, and end-to-end multi-node tests.
