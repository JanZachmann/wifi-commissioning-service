# wifi-commissioning-service

WiFi provisioning daemon for omnect OS. It offers scan, connect and network
management over BLE GATT (for mobile apps) and a REST API on a Unix socket
(for edge modules and local tools).

## Constraints

- Feature flags: `systemd` (sd-notify readiness) and `ble` (BLE transport
  with `bluer`). Run clippy and tests also with `--all-features`, so the
  code behind `ble` is checked.
- All WiFi operations go through the `WifiBackend` trait
  (`src/backend/wifi_backend.rs`). Transport and core code never call
  `wifi-ctrl` or system interfaces directly. A new trait method also needs
  an implementation in `src/backend/mock_backend.rs`.
- On connect, credentials go to the volatile wpa_supplicant config first.
  `SAVE_CONFIG` runs only after the `CTRL-EVENT-CONNECTED` event, never
  before.
- BLE scan results and saved-network lists are read in 100-byte chunks
  (`SCAN_RESULT_CHUNK_SIZE` in `src/transport/ble/uuids.rs`). A change to
  their serialization must keep that limit.
- The socket path is `/run/wifi-commissioning-service/<interface>/api.sock`
  and must match the templated unit `systemd/wifi-commissioning-service@.socket`.
- Tests live in `#[cfg(test)]` modules in the source files. There is no
  `tests/` directory.

Manual API test: `examples/unix-socket-client/wifi-client.sh <command>`
(`scan`, `list`, `connect`, `status`, `version`, `service-info`,
`disconnect`, `list-saved`, `forget`).

## Breaking UDS API changes

Edge modules use the Unix socket API of this service. Before `gh pr create`
or a PR title change, check the PR diff for a breaking API change as defined
in [README.md, "Breaking changes"](README.md#breaking-changes). If it is
breaking:

- put `!` in the PR title (e.g. `feat!: ...`); the release notes take it
  from there,
- bump at least the minor version while on 0.x,
- describe the old and new format in the PR description.

The hook `.claude/hooks/uds-api-guard.sh` blocks the PR command until this
is decided. It checks changes in `src/` and the socket unit. If the change is
not breaking, put `UDS_API_BREAKING=no` directly before `gh` and tell the
user why.
