# Changelog

The About button in the ribbon shows the newest entry, so keep the top
section short and written for someone using the plugin, not maintaining it.

## 0.8.0

- About button: version, build toolchain, and what changed, on the command line.
- Feedback button: opens the issue tracker, and prints the link in case no
  browser opens.

## 0.7.0

- Block references are expanded to real geometry. A selection of nothing but
  blocks used to come back empty; nested blocks are followed too.
- The drawing's `insertion_units` now travels with the selection, so callers no
  longer guess the unit or read it from a file.
- Blocks with non-uniform scaling are reported as skipped rather than silently
  distorted, since a scaled circle is an ellipse.

## 0.6.0

- Targets Open CAD Studio v0.9.8 and its `SelectionChangedV4`, which carries a
  `tab_id` so clients can tell drawings apart. The older tab-less notification
  is still accepted.
- `status` reports the rustc that built the plugin. Rust has no stable ABI, so a
  mismatch with the host kills the plugin on the first compound call while
  simple calls still work — this makes that one comparison instead of a hunt.
- A blocked host call no longer wedges the whole bridge.

## 0.5.0

- Connects itself: the host routes every command through every plugin, so the
  first command you run hands the plugin its write channel. Connect became a
  fallback.
- Status button, printing address, connection state, selection size and event
  count.
- Text and MText are read again, including their contents.

## 0.4.0

- Renamed to MCP Bridge, published under GPL-3.0, and listed in the plugin
  registry.

## 0.3.0

- Writing: draw and delete entities, each batch as a single undo step.
- Request/response protocol with several requests per connection.

## 0.2.0

- Reads the live selection with full geometry: lines, arcs, circles, polylines
  with exact bulges, ellipses, text.

## 0.1.0

- First working bridge: a socket inside Open CAD Studio, serving the selection
  to tools outside it.
