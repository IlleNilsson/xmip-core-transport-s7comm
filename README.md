# xmip-core-transport-s7comm

S7comm transport: Siemens S7 over ISO-on-TCP — setup communication, read and write of data blocks, inputs, outputs and flags by address — a Location reads or writes a PLC area as a Stream. A technology of [xmip-core-transport](https://github.com/IlleNilsson/xmip-core-transport).

A Send Location writes on a session set up once per CPU and kept (`transport::Pool`) — a CPU serves few connections, and each costs it one. Until 2026-09-27 every write connected, set up communication and disconnected.

A Receive Location polls on the same kept session. Until 2026-09-28 every receive connected, set up communication and disconnected. The configured address is parsed once, when the transport is made; until the same day every receive parsed it again.

A send target is read by `net::Target` in [xmip-core-library-net](https://github.com/IlleNilsson/xmip-core-library-net), the one reading of a URI every technology calls: scheme, authority, path and decoded query. Until 2026-09-28 it was read through the transport capability's `socket::target`, which split it on its first slash and left the query in the path.

## Toolchain

`rust-toolchain.toml` pins the toolchain for the whole estate. Do not change it
here.

## Verification

The included workflow is manual-only and calls the versioned shared workflow at
`IlleNilsson/.github@v1`.
