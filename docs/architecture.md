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
domain <- protocol <- transport
   ^                    ^
   |                    |
platform              tevir
                         ^
                         |
                     identity
```

`domain` is deterministic and performs no I/O. `protocol` owns serialization,
limits, and wire validation. `platform` translates native input to and from
domain events without depending on networking. `identity` owns local
credentials and explicit peer trust. `transport` owns authenticated QUIC
connections, stream separation, and network deadlines. The `tevir` executable
owns configuration, process lifecycle, and session orchestration.

The desktop surface uses `eframe` and `egui`. Its pairing and validation
actions call the same identity and configuration boundaries as non-graphical
commands. Linux builds enable only eframe's Wayland window backend.

Native handles, portal objects, Windows messages, and operating-system key
codes must remain inside `platform`. Wire DTOs must remain inside `protocol`.

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
or X11 compatibility.

Windows capture and injection use low-level hooks on a dedicated message-loop
thread and `SendInput`. Native engines remain behind Tevir's bounded service
queues and platform-neutral input model.

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

## State And Backpressure

The controller is the sole authority for focus epochs and topology. Each input
batch carries its epoch and a monotonically increasing sequence. An agent
acknowledges the highest contiguous sequence it applied. It rejects stale
epochs and releases held state when a stream breaks.

Topology rectangles use controller-global coordinates. An edge transition
converts the entry point to destination-local coordinates before it enters a
protocol message.

Key and button transitions are never dropped. Pointer motion may be coalesced
before serialization. Every queue and clipboard payload is bounded; overload
disconnects or degrades pointer sampling instead of growing memory without a
limit.
