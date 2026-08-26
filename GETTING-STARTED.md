# Getting started with MCP Bridge

MCP Bridge is a small plugin that lets tools outside Open CAD Studio — an MCP
server, a Python script, anything that can open a TCP socket — **read what you
have selected and draw into the drawing you have open**.

It exists because of a practical wall: a drawing that is open in the GUI is
locked by Windows. An external tool cannot read it, and cannot even copy it.
The plugin sidesteps that by living *inside* the process that holds the lock
and answering questions over a local socket.

---

## What works today

Read this table before you install; one feature depends on a host change that
has not shipped yet.

| | Official release (v0.9.7) | Build with the selection patch |
|---|---|---|
| Draw into the open drawing | yes | yes |
| Delete entities | yes | yes |
| Print messages on the command line | yes | yes |
| **See what the user selected** | **no** | yes |

Open CAD Studio does not yet tell plugins when the selection changes, so on a
stock build `{"op":"selection"}` always comes back empty. Two issues track this:

* [#879](https://github.com/HakanSeven12/OpenCADStudio/issues/879) — the report,
  with a minimal patch
* [#881](https://github.com/HakanSeven12/OpenCADStudio/pull/881) — the
  maintainers' own, broader implementation (`SelectionChangedV4`, carrying a
  tab id). Open at the time of writing.

Once #881 is merged and released, selection tracking works on the official
build — but note the variant is named `SelectionChangedV4`, not
`SelectionChanged`, so **this plugin will need a matching update**. Watch the
releases here rather than assuming it starts working by itself.

---

## Install

**From the Plugin Manager (recommended).** MCP Bridge is in the curated
registry, so it shows up in *Plugin Manager → marketplace*. Install it there and
restart Open CAD Studio.

**Manually.** Download `plugin.toml` and the library for your platform from the
[latest release](https://github.com/piyton/ocs-mcp-bridge/releases), and put
them side by side in:

```
Windows   %APPDATA%\OpenCADStudio\plugins\ocs.mcp_bridge\
Linux     ~/.config/OpenCADStudio/plugins/ocs.mcp_bridge/
macOS     ~/Library/Application Support/OpenCADStudio/plugins/ocs.mcp_bridge/
```

Restart, and you should see an **MCP Bridge** tab in the ribbon.

---

## First run

There is nothing to configure. Click **Status** in the MCP Bridge tab and the
command line prints something like:

```
MCP Bridge 0.5.0
  address    127.0.0.1:48810
  channel    connected - reading and writing work
  selection  12 entities
  events     4 selection notifications
```

That is the whole health check:

* **address** — where your tool connects. Override the port with the
  `OCS_BRIDGE_PORT` environment variable before starting Open CAD Studio.
* **channel** — whether the plugin holds the host's write channel. It grabs
  that the first time *any* command runs, so it usually says connected
  already. If not, run any command (or click **Connect**).
* **events** — how many selection changes have arrived. If this stays at 0,
  your build does not have the selection patch; see the table above.

---

## How it works

```
   your tool  ──JSON over TCP──▶  MCP Bridge  ──▶  Open CAD Studio
                                  (plugin)          (the open drawing)
```

The plugin runs in its own OS process that the host launches, connected back to
the host over a private channel. A crash in the plugin cannot take your drawing
with it.

It is deliberately a *thin* layer. It offers a handful of stable primitives and
holds no opinions about what you do with them — all the logic lives in your
tool. That means the plugin almost never has to change, and you almost never
have to restart Open CAD Studio while developing against it. Anyone who has
worked with pyRevit's routes extension will recognise the shape.

Two details worth knowing, because they explain the design:

* Geometry can only be read while the host is calling into the plugin. The
  plugin therefore asks the host for a document snapshot on demand, rather than
  keeping a copy around.
* Coordinates are passed through **raw, in drawing units** — nothing is
  silently converted. Many drawings do not record their unit at all
  (`$INSUNITS = 0`), so guessing would risk being off by a factor of 1000.
  Convert on your side, deliberately.

---

## Protocol

JSON lines on `127.0.0.1:48810`. Send one object, read one object back. You can
send several requests over the same connection.

| Request | What it does |
|---|---|
| `{"op":"status"}` | version, connection state, selection size, event count |
| `{"op":"selection"}` | the selected entities, with geometry |
| `{"op":"add","entities":[…],"undo_label":"…"}` | draws; returns the new handles |
| `{"op":"remove","handles":[…],"undo_label":"…"}` | deletes those entities |
| `{"op":"info","msg":"…"}` | prints a line on the command line |

Every write is wrapped in a single undo step, so **Ctrl+Z in the GUI reverts the
whole batch** — including a hundred entities added at once.

### Entity shapes

Angles are in radians; arcs run counter-clockwise from start to end.

```json
{"type":"line",       "start":[x,y], "end":[x,y], "layer":"0"}
{"type":"arc",        "center":[x,y], "radius":r, "start_angle":a, "end_angle":b}
{"type":"circle",     "center":[x,y], "radius":r}
{"type":"lwpolyline", "vertices":[[x,y,bulge], …], "closed":true}
```

Those four you can both read and write. Reading also returns:

* `ellipse` — centre, major axis, minor ratio, parameter range
* `insert` — block name, insertion point, rotation
* `text` / `mtext` — the string, position, height

Legacy `POLYLINE` and `POLYLINE2D` entities are normalised to `lwpolyline`, with
their bulges preserved, so you only have one polyline shape to handle.

The `bulge` on a polyline vertex is what keeps an arc an *arc*. Do not flatten
it into short segments unless you have to — a swept profile built from
tessellated line work is both heavier and less accurate.

---

## Quick start in Python

```python
import json, socket

def bridge(payload, port=48810):
    s = socket.create_connection(("127.0.0.1", port), timeout=30)
    try:
        s.sendall((json.dumps(payload) + "\n").encode())
        buf = b""
        while not buf.endswith(b"\n"):
            chunk = s.recv(1 << 20)
            if not chunk:
                break
            buf += chunk
    finally:
        s.close()
    return json.loads(buf)

print(bridge({"op": "status"}))

# draw a 68 x 78 rectangle with a hole, as one undo step
res = bridge({
    "op": "add",
    "undo_label": "frame test",
    "entities": [
        {"type": "lwpolyline", "closed": True,
         "vertices": [[0, 0, 0], [68, 0, 0], [68, 78, 0], [0, 78, 0]]},
        {"type": "circle", "center": [34, 39], "radius": 6},
    ],
})
print(res)                       # {'ok': True, 'drawn': 2, 'handles': [...]}

bridge({"op": "remove", "handles": res["handles"]})
```

Any language works — it is a socket and a line of JSON.

---

## Troubleshooting

| What you see | What it means |
|---|---|
| Connection refused | Open CAD Studio is not running, or the plugin is not installed. Check for the MCP Bridge ribbon tab. |
| `not connected yet: click Connect once…` | The plugin has not been handed the write channel. Run any command, or click **Connect**. |
| `selection` returns 0 entities, `events` is 0 | Your build does not emit selection changes; see the table at the top. |
| `unknown entity type "…"` on a write | Only line, arc, circle and lwpolyline can be written. |
| Bind failure on startup | Another instance already holds the port. Check `%TEMP%/ocs_mcp_bridge_socket_error.txt`, or set `OCS_BRIDGE_PORT`. |

Two files in your temp directory report what the socket did at startup:
`ocs_mcp_bridge_socket_ok.txt` and `ocs_mcp_bridge_socket_error.txt`.

---

## Security

The socket listens on `127.0.0.1` only, so nothing on the network can reach it.
Anything that *can* connect — any process on your machine — can modify the open
drawing. The changes are undoable, but do not forward the port.

---

## Licence

GPL-3.0-only, the same as the host it links against. Source and issues:
https://github.com/piyton/ocs-mcp-bridge
