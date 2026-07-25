# Tevir

Tevir is a network software KVM for Windows and Linux
Wayland. A controller captures local keyboard and pointer input, moves focus
through a configured screen topology, and delivers typed events to an agent on
the focused node.

## Workspace

- `domain`: node identities, geometry, input events, and topology routing.
- `discovery`: bounded local-network discovery for explicit peer pairing.
- `identity`: persistent local credentials, pairing bundles, and trusted peers.
- `protocol`: handshake/session messages and bounded Postcard framing.
- `transport`: mutually authenticated QUIC sessions and isolated bulk streams.
- `session`: controller focus routing and ordered agent input delivery.
- `telemetry`: structured file, stderr, and desktop diagnostics.
- `platform`: native input services and Windows/Wayland host probes.
- `tevir`: the executable, validated TOML configuration, and diagnostics.

## Development

The pinned Rust toolchain is installed automatically by `rustup`.

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo build --workspace
```

Inspect the current desktop session:

```sh
cargo run -p tevir
cargo run -p tevir -- doctor
cargo run -p tevir -- doctor --json
```

The desktop control surface creates the local node identity, finds nearby nodes,
manages explicit peer pairing, validates configuration files, and reports
native input prerequisites.

Validate one of the portable example configurations:

```sh
cargo run -p tevir -- check examples/controller.toml
cargo run -p tevir -- check examples/agent.toml
```

See [architecture.md](docs/architecture.md) for the component boundaries and
[ROADMAP.md](ROADMAP.md) for the implementation order.

## License

Tevir is licensed under the GNU General Public License v3.0 only. See
[LICENSE](LICENSE).
