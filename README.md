# MCP Bridge for Open CAD Studio

A plugin that lets external tools — MCP servers, scripts, anything that can
open a TCP socket — **read the live selection and write into the open
drawing** of [Open CAD Studio](https://github.com/HakanSeven12/OpenCADStudio).

Why: a drawing that is open in the GUI is file-locked, so external tools
cannot even copy it. This plugin sits inside the process that holds the lock
and serves the data over `127.0.0.1` instead.

The dll is deliberately a thin, generic shim (same model as pyRevit's routes
extension): all domain logic lives on the client side, so evolving a workflow
never requires recompiling the plugin or restarting Open CAD Studio.

## Install

New here? Start with **[GETTING-STARTED.md](GETTING-STARTED.md)** - install,
first run, protocol and troubleshooting in one place.


Via the Plugin Manager: add the repository `piyton/ocs-mcp-bridge` as a
manual link, or install from a release: put `plugin.toml` and the platform
library side by side in
`<config>/OpenCADStudio/plugins/ocs.mcp_bridge/` and restart.

The bridge connects itself: the host routes every command through every
plugin, so the first command you run in the session - any command - hands the
plugin its write channel. The ribbon tab has **Status**, which prints the
address, connection state, selection size and event count to the command line,
and **Connect** as an explicit fallback.

## Protocol

JSON lines on `127.0.0.1:48810` (override with `OCS_BRIDGE_PORT`), multiple
requests per connection, one JSON object in, one out:

| request | reply |
|---|---|
| `{"op":"status"}` | `{"ok":true,"connected":true,"selected":40,...}` |
| `{"op":"selection"}` | handles + full geometry, block references expanded, plus `insertion_units` from the live document |
| `{"op":"add","entities":[...],"undo_label":"..."}` | draws entities, returns their handles |
| `{"op":"remove","handles":[...],"undo_label":"..."}` | deletes entities |
| `{"op":"info","msg":"..."}` | prints on the command line |

Every write is wrapped in an undo step: Ctrl+Z in the GUI reverts the whole
batch. Coordinates are raw drawing units (deliberately no silent unit
conversion); angles are radians, arcs CCW.

Entity JSON, both directions:

```json
{"type":"line","start":[x,y],"end":[x,y],"layer":"0"}
{"type":"arc","center":[x,y],"radius":r,"start_angle":a,"end_angle":b}
{"type":"circle","center":[x,y],"radius":r}
{"type":"lwpolyline","vertices":[[x,y,bulge],...],"closed":true}
```

Reading additionally returns `ellipse` (centre, major axis, ratio, params)
and `insert` (block name, insert point, rotation); legacy `Polyline`/
`Polyline2D` are normalised to `lwpolyline` with exact bulges.

## Compatibility

**Build the plugin with the same rustc the host was built with.** Rust has no
stable ABI, and this plugin passes `EntityType` and `CadDocument` across the
dynamic-library boundary. A mismatch does not fail cleanly: the plugin loads,
`status` and `info` work, and then the runner process dies the moment anything
compound crosses - which looks like a host bug and is not one.

Diagnose it in one line. `{"op":"status"}` reports the compiler that built the
plugin; compare it with the host:

```
{"op":"status"}  ->  "rustc": "1.98.0 (88d9e12ae…)"
strings "…/OpenCADStudio.exe" | grep -o 'rustc/[0-9a-f]\{40\}'
```

Upstream CI builds releases with `dtolnay/rust-toolchain@stable`, so the
required toolchain moves with each Rust release, roughly every six weeks.
`rustup update stable` before rebuilding is usually all it takes.


Built against the `v0.9.8` tag, mirroring the workspace `[patch]` so
`acadrust` resolves to the same revision the host uses. The host's version gate
checks the API major and the `acadrust` source, but **not** the compiler - see
the toolchain note above.

Live selection tracking needs `SelectionChangedV4`, which the host emits from
**v0.9.8** onwards ([#879](https://github.com/HakanSeven12/OpenCADStudio/issues/879),
implemented in [#881](https://github.com/HakanSeven12/OpenCADStudio/pull/881)).
That notification carries a `tab_id`, which the bridge passes through so a
client can tell which drawing a selection belongs to. On older hosts the
tab-less `SelectionChanged` is still accepted, so selections keep working
wherever the host emits either form.

## Security

The socket binds to `127.0.0.1` only. Anything that can connect to it can
modify the open drawing (undoably); do not forward the port.

## License

GPL-3.0-only, like the host it links against.
