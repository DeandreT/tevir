# Architecture

## Terminology

- A **node** is one participating computer with one stable `NodeId`.
- The **controller** owns the screen topology and decides which node has focus.
- An **agent** receives input when its node is focused. A later release may let
  one installation switch roles, but each live session has one controller.
- A **focus epoch** identifies one focus ownership interval. Events from an old
  epoch must never be injected after focus changes.

These terms are project-owned and independent of legacy application naming and
behavior.

## Dependency Direction

```text
tevir     -> discovery, domain, identity, platform, protocol, session, telemetry, transport
discovery -> domain, identity, protocol
transport -> domain, identity, protocol
session   -> domain, protocol
identity  -> domain
platform  -> domain
protocol  -> domain
```

`domain` is deterministic and performs no I/O. `protocol` owns serialization,
limits, and wire validation. `platform` translates native input and clipboard
operations without depending on networking. `identity` owns local credentials
and explicit peer trust. `transport` owns authenticated QUIC connections,
stream separation, and network deadlines. `session` owns the deterministic
controller and agent state machines. The `tevir` executable owns configuration,
process lifecycle, discovery polling, and session orchestration.

`discovery` publishes and browses a DNS-SD service on the local network. Records
carry the exact protocol version, platform, capabilities, certificate
fingerprint, and a size-limited copy of the public pairing bundle. Received
records are validated before entering a bounded nearby-node registry. Discovery
never changes the trust store: pairing still requires the verification code
from an independent channel.

The desktop surface uses `eframe` and `egui`. Its pairing and validation
actions call the same identity and configuration boundaries as non-graphical
commands. A bounded background runtime owns the authenticated connections and
native input services, while the GUI consumes lifecycle, connection, and focus
events. Linux builds enable only eframe's Wayland window backend.

Native handles, portal objects, Windows messages, and operating-system key
codes must remain inside `platform`. Wire DTOs must remain inside `protocol`.

## Observability

Runtime components emit structured `tracing` events at lifecycle, focus,
security, backpressure, and failure boundaries. The executable writes those
events to stderr, seven retained daily files, and a bounded in-memory buffer
shown in the desktop Logs view. Pairing material, credentials, clipboard
contents, and individual input events must never enter logs.

## Input Model

Keys use USB HID usage page and usage identifiers rather than a Windows virtual
key or Linux evdev code. Pointer movement distinguishes absolute and relative
coordinates. Scrolling distinguishes discrete wheel detents from
high-resolution motion. Key repeat is an explicit action, which prevents a
release from carrying a contradictory repeat count.

Native backends run their required event loops on dedicated threads and
communicate with orchestration through bounded queues. When a focus epoch ends,
the receiving backend releases all held input before accepting a new epoch.

## Platform Boundaries

Linux support is Wayland-only. Capture and injection use desktop portals and
EIS instead of compositor-specific protocols, privileged `/dev/input` access,
or X11 compatibility. Clipboard access uses the XDG Clipboard portal attached
to a RemoteDesktop session. Portal reads and writes have fixed deadlines and
accept only bounded UTF-8 `text/plain` selections.

Windows capture and injection use low-level hooks on a dedicated message-loop
thread and `SendInput`. Clipboard access uses the native Windows clipboard and
suppresses notifications caused by its own writes. Native engines remain behind
Tevir's bounded service queues and platform-neutral input model.

## Protocol

The protocol uses a four-byte big-endian payload length followed by one
Postcard envelope. A codec has a configurable limit capped by a hard 16 MiB
ceiling; the default is 1 MiB. Input batches are non-empty and contain at most
512 events.

Protocol versions match exactly. Version mismatch is a handshake result, not a
request to enter a legacy mode. The protocol does not reuse legacy messages or
configuration.

Framing is not security. Each installation has a persistent private credential
and exports only its trust anchor in a pairing bundle. Pairing requires an
out-of-band fingerprint comparison before that anchor is stored.

Live traffic uses TLS 1.3 over QUIC with certificates required from both
parties. The certificate chain is bound to the claimed `NodeId` during an
exact-version, nonce-bearing application handshake. Control messages use a
high-priority bidirectional stream; bulk clipboard payloads use separate,
lower-priority streams. Stream counts, flow-control windows, handshakes,
frames, operation deadlines, idle timeouts, and reconnect attempts are all
bounded.

## Clipboard

Clipboard synchronization begins with bounded UTF-8 text. Each local change
gets a generation containing its owner `NodeId` and a monotonic sequence. A
small control offer carries the generation, byte length, and digest; the typed
payload travels on a lower-priority clipboard stream and is verified against
the offer before native application.

Control and payload streams may arrive in either order. Each peer session keeps
one native application in flight and at most one newer inbound generation;
newer updates replace incomplete older ones. An applied message is emitted only
after the platform confirms its write. The corresponding native change
notification is suppressed so received content is not sent back to its owner.
Native workers expose bounded command and event queues and never put clipboard
contents in logs. Platform MIME and format details terminate at the native
boundary; only validated UTF-8 text enters the protocol.

## State And Backpressure

The controller is the sole authority for focus epochs and topology. Each input
batch carries its epoch and a monotonically increasing sequence. An agent
acknowledges the highest contiguous sequence only after native application is
confirmed. Duplicate batches repeat the last acknowledgement without
reinjection; gaps and stale epochs are rejected. Focus changes, display changes,
and broken connections release held state before another epoch can inject
input.

Topology rectangles use controller-global coordinates. An edge transition
converts the entry point to destination-local coordinates before it enters a
protocol message. The controller retains subpixel pointer motion, supports
negative global coordinates, and only crosses between rectangles that share an
edge.

The controller keeps one input batch in flight per agent and coalesces newer
pointer motion until native application is acknowledged. Key and button
transitions are never dropped, and pointer motion is never coalesced across a
key, button, or scroll boundary. Every queue and clipboard payload is bounded;
overload produces an explicit backpressure condition instead of growing memory
without a limit.
