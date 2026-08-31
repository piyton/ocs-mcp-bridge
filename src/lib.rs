//! MCP Bridge - a generic read/write shim between Open CAD Studio and
//! external tooling (MCP servers, scripts).
//!
//! Design principle: this cdylib is deliberately dumb. All domain logic lives
//! on the client side; the plugin only offers stable primitives. That way the
//! dll rarely needs to change - and Open CAD Studio rarely needs a restart -
//! when a workflow evolves. Same model as pyRevit's routes extension.
//!
//! Protocol on 127.0.0.1:`OCS_BRIDGE_PORT` (default 48810), JSON lines,
//! multiple requests per connection:
//!
//!   {"op":"status"}                      -> connected? selection count
//!   {"op":"selection"}                   -> handles + geometry, block
//!                                           references expanded to world
//!                                           space, plus the document's
//!                                           `insertion_units`
//!   {"op":"add","entities":[...],
//!    "undo_label":"..."}                 -> draws into the open drawing
//!   {"op":"remove","handles":[...],
//!    "undo_label":"..."}                 -> deletes entities
//!   {"op":"info","msg":"..."}            -> message on the command line
//!
//! Entity JSON (angles in radians, arcs CCW from start to end):
//!   {"type":"line","start":[x,y],"end":[x,y],"layer"?}
//!   {"type":"arc","center":[x,y],"radius":r,"start_angle":a,"end_angle":b}
//!   {"type":"circle","center":[x,y],"radius":r}
//!   {"type":"lwpolyline","vertices":[[x,y,bulge],...],"closed":bool}
//!
//! The sender (thread-safe write channel to the host) is cached on the first
//! dispatch of *any* command - the host routes every command through every
//! plugin - so the bridge connects itself as soon as the user does anything.
//! The Connect button is only a fallback. Live selection tracking needs the
//! host to emit a selection notification, which it does from v0.9.8 onwards
//! (see <https://github.com/HakanSeven12/OpenCADStudio/issues/879>); writing
//! and snapshot reads work on older hosts too.
//!
//! Host and plugin must be built by the same rustc: Rust has no stable ABI and
//! compound types cross the dll boundary. `status` reports the compiler that
//! built this plugin so a mismatch is one comparison away.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use acadrust::entities::{Arc as CadArc, Circle, EntityType, Line, LwPolyline, LwVertex};
use acadrust::types::{Vector2, Vector3};
use ocs_plugin_api::host::{
    BuiltinPlugin, HostApi, HostNotification, PluginRequestSender,
};
use ocs_plugin_api::ipc::protocol::{PluginRequest, PluginResponse};
use ocs_plugin_api::manifest::{ApiVersion, PluginManifest};
use ocs_plugin_api::ribbon::{CadModule, IconKind, ModuleEvent, RibbonGroup, RibbonItem, ToolDef};
use serde_json::{json, Value};

static MANIFEST: PluginManifest = PluginManifest {
    id: "ocs.mcp_bridge",
    name: "MCP Bridge",
    version: "0.9.0",
    description: "Read/write bridge for MCP servers and external tools.",
    api_version: ApiVersion::CURRENT,
    ribbon_order: 60,
    xdata_apps: &[],
    command_prefixes: &["MCP_"],
};

/// Handles from the most recent selection notification.
static LAST_SELECTION: Mutex<Vec<u64>> = Mutex::new(Vec::new());
/// Tab the selection belongs to. `None` on hosts that send the tab-less
/// notification, which is why clients must treat it as optional.
static SELECTION_TAB: Mutex<Option<u64>> = Mutex::new(None);
/// Path of the active document, refreshed whenever the host calls in.
///
/// `HostApi::document_path` is only reachable during a dispatch or `on_load`,
/// so this is a cache rather than a live read: it can lag if the user switches
/// tabs without running a command. Clients get it as a best-effort hint.
static DOCUMENT_PATH: Mutex<Option<String>> = Mutex::new(None);
/// Geometry captured via the Connect button (fallback without a sender).
static SELECTION_JSON: Mutex<Option<String>> = Mutex::new(None);
/// Thread-safe write channel to the host, cached on the first dispatch.
///
/// Held as an `Arc` so a caller can clone it out and release the lock before
/// blocking on the host. Holding the lock across the request would let one
/// slow or unanswered call wedge every other request, `status` included -
/// which is exactly what happens on a headless host, where nothing services
/// plugin requests at all.
static SENDER: Mutex<Option<Arc<dyn PluginRequestSender>>> = Mutex::new(None);

static NOTIF_TOTAL: AtomicUsize = AtomicUsize::new(0);
static NOTIF_SELECTION: AtomicUsize = AtomicUsize::new(0);
static SERVER: OnceLock<()> = OnceLock::new();


/// Where to report problems. Also printed, so a headless or locked-down
/// machine still gets a usable link.
const ISSUES_URL: &str = "https://github.com/piyton/ocs-mcp-bridge/issues";

/// The changelog travels inside the binary, so About always describes the
/// build you are actually running rather than whatever is on disk.
const CHANGELOG: &str = include_str!("../CHANGELOG.md");

/// The newest changelog section, without its heading.
///
/// Entries are `## <version>` followed by bullets; take the first one and stop
/// at the next heading. Returns an empty slice if the file is not shaped that
/// way, so a malformed changelog costs a blank panel rather than a panic.
fn latest_changes() -> Vec<&'static str> {
    let mut lines = CHANGELOG.lines().skip_while(|l| !l.starts_with("## "));
    lines.next();
    lines
        .take_while(|l| !l.starts_with("## "))
        .map(str::trim_end)
        .filter(|l| !l.is_empty())
        .collect()
}

/// Ask the desktop to open a URL. Best-effort: the caller prints the link too.
fn open_url(url: &str) {
    #[cfg(target_os = "windows")]
    let cmd = std::process::Command::new("cmd").args(["/C", "start", "", url]).spawn();
    #[cfg(target_os = "macos")]
    let cmd = std::process::Command::new("open").arg(url).spawn();
    #[cfg(all(unix, not(target_os = "macos")))]
    let cmd = std::process::Command::new("xdg-open").arg(url).spawn();
    let _ = cmd;
}


/// The About panel, as a page.
///
/// The plugin API has no dialog: `ModuleEvent` offers commands and file
/// pickers, and `HostApi` only writes lines to the command line - which shows
/// two at a time and scrolls away. Writing a page and handing it to the
/// browser is the only way to show this much text legibly, and it keeps the
/// content tied to the running build rather than a file on disk.
const ABOUT_TEMPLATE: &str = r#"<!doctype html>
<html lang="en"><head><meta charset="utf-8">
<title>MCP Bridge __VERSION__</title>
<style>
  :root {
    color-scheme: light dark;
    --ink: #14181d; --dim: #5b6672; --line: #d8dee6;
    --bg: #f7f9fb; --card: #ffffff; --accent: #1f6feb;
  }
  @media (prefers-color-scheme: dark) {
    :root { --ink: #e6edf3; --dim: #93a1b0; --line: #2b333c;
            --bg: #0f1419; --card: #161b22; --accent: #58a6ff; }
  }
  * { box-sizing: border-box; }
  body { margin: 0; padding: 2.5rem 1.5rem; background: var(--bg); color: var(--ink);
         font: 15px/1.6 system-ui, -apple-system, "Segoe UI", sans-serif; }
  main { max-width: 46rem; margin: 0 auto; }
  h1 { font-size: 1.5rem; margin: 0 0 .2rem; letter-spacing: -.01em; }
  .sub { color: var(--dim); margin: 0 0 2rem; }
  .card { background: var(--card); border: 1px solid var(--line);
          border-radius: 10px; padding: 1.25rem 1.4rem; margin-bottom: 1.25rem; }
  h2 { font-size: .78rem; text-transform: uppercase; letter-spacing: .09em;
       color: var(--dim); margin: 0 0 .9rem; font-weight: 600; }
  dl { display: grid; grid-template-columns: 9.5rem 1fr; gap: .5rem 1rem; margin: 0; }
  dt { color: var(--dim); }
  dd { margin: 0; font-family: ui-monospace, "Cascadia Code", Consolas, monospace;
       font-size: .88rem; word-break: break-all; }
  ul { margin: 0; padding-left: 1.1rem; }
  li { margin-bottom: .45rem; }
  a { color: var(--accent); }
  footer { color: var(--dim); font-size: .85rem; margin-top: 2rem; }
</style></head><body><main>
  <h1>MCP Bridge __VERSION__</h1>
  <p class="sub">Reads the live selection and draws into the open drawing,
     for tools outside Open CAD Studio.</p>

  <div class="card">
    <h2>New in this version</h2>
    <ul>__CHANGES__</ul>
  </div>

  <div class="card">
    <h2>This build</h2>
    <dl>
      <dt>Bridge address</dt><dd>127.0.0.1:__PORT__</dd>
      <dt>Write channel</dt><dd>__CONNECTED__</dd>
      <dt>Selection</dt><dd>__SELECTED__ entities</dd>
      <dt>Built by</dt><dd>rustc __RUSTC__</dd>
    </dl>
    <p style="color:var(--dim);font-size:.85rem;margin:.9rem 0 0">
      Host and plugin must be built by the same rustc. If reading a selection
      makes the bridge disappear while status still works, that is the cause.</p>
  </div>

  <div class="card">
    <h2>Links</h2>
    <ul>
      <li><a href="__REPO__">Source and documentation</a></li>
      <li><a href="__REPO__/blob/main/GETTING-STARTED.md">Getting started</a></li>
      <li><a href="__ISSUES__">Report a problem or request a feature</a></li>
    </ul>
  </div>

  <footer>GPL-3.0-only, like the host it links against.</footer>
</main></body></html>"#;

/// Render the About page and return where it was written.
fn about_html() -> Option<std::path::PathBuf> {
    let changes: String = latest_changes()
        .iter()
        .map(|l| {
            let text = l.trim_start_matches(['-', ' ']);
            format!("<li>{}</li>", html_escape(text))
        })
        .collect();
    let port = std::env::var("OCS_BRIDGE_PORT").unwrap_or_else(|_| "48810".into());
    let connected = SENDER.lock().map(|g| g.is_some()).unwrap_or(false);
    let selected = LAST_SELECTION.lock().map(|g| g.len()).unwrap_or(0);
    let repo = ISSUES_URL.trim_end_matches("/issues");

    let page = ABOUT_TEMPLATE
        .replace("__VERSION__", MANIFEST.version)
        .replace("__CHANGES__", &changes)
        .replace("__PORT__", &port)
        .replace(
            "__CONNECTED__",
            if connected { "connected" } else { "not connected - run any command" },
        )
        .replace("__SELECTED__", &selected.to_string())
        .replace("__RUSTC__", env!("BRIDGE_RUSTC"))
        .replace("__ISSUES__", ISSUES_URL)
        .replace("__REPO__", repo);

    let path = std::env::temp_dir().join("ocs_mcp_bridge_about.html");
    std::fs::write(&path, page).ok()?;
    Some(path)
}

/// Minimal escaping: the changelog is ours, but it must not be able to break
/// the page if someone writes an ampersand or angle bracket in it.
fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

// ---------------------------------------------------------------------------
// geometry: EntityType -> JSON (reading)
// ---------------------------------------------------------------------------

fn entity_json(e: &EntityType) -> Option<Value> {
    let v = match e {
        EntityType::Line(l) => json!({
            "type": "line", "handle": l.common.handle.value(),
            "layer": l.common.layer,
            "start": [l.start.x, l.start.y], "end": [l.end.x, l.end.y],
        }),
        EntityType::Arc(a) => json!({
            "type": "arc", "handle": a.common.handle.value(),
            "layer": a.common.layer,
            "center": [a.center.x, a.center.y], "radius": a.radius,
            "start_angle": a.start_angle, "end_angle": a.end_angle,
        }),
        EntityType::Circle(c) => json!({
            "type": "circle", "handle": c.common.handle.value(),
            "layer": c.common.layer,
            "center": [c.center.x, c.center.y], "radius": c.radius,
        }),
        EntityType::Ellipse(el) => json!({
            "type": "ellipse", "handle": el.common.handle.value(),
            "layer": el.common.layer,
            "center": [el.center.x, el.center.y],
            "major_axis": [el.major_axis.x, el.major_axis.y],
            "minor_ratio": el.minor_axis_ratio,
            "start_param": el.start_parameter, "end_param": el.end_parameter,
        }),
        EntityType::LwPolyline(p) => json!({
            "type": "lwpolyline", "handle": p.common.handle.value(),
            "layer": p.common.layer, "closed": p.is_closed,
            "elevation": p.elevation,
            "vertices": p.vertices.iter()
                .map(|v| json!([v.location.x, v.location.y, v.bulge]))
                .collect::<Vec<_>>(),
        }),
        // Legacy heavy polylines: older drawings often carry their closed
        // contours in these rather than in LwPolyline.
        EntityType::Polyline2D(p) => json!({
            "type": "lwpolyline", "handle": p.common.handle.value(),
            "layer": p.common.layer, "closed": p.flags.is_closed(),
            "elevation": p.elevation,
            "vertices": p.vertices.iter()
                .map(|v| json!([v.location.x, v.location.y, v.bulge]))
                .collect::<Vec<_>>(),
        }),
        EntityType::Polyline(p) => json!({
            "type": "lwpolyline", "handle": p.common.handle.value(),
            "layer": p.common.layer, "closed": p.flags.is_closed(),
            "elevation": 0.0,
            "vertices": p.vertices.iter()
                .map(|v| json!([v.location.x, v.location.y, 0.0]))
                .collect::<Vec<_>>(),
        }),
        EntityType::Insert(i) => json!({
            "type": "insert", "handle": i.common.handle.value(),
            "layer": i.common.layer, "block": i.block_name,
            "insert_point": [i.insert_point.x, i.insert_point.y],
            "rotation": i.rotation,
        }),
        EntityType::Text(t) => json!({
            "type": "text", "handle": t.common.handle.value(),
            "layer": t.common.layer, "value": t.value,
            "position": [t.insertion_point.x, t.insertion_point.y],
            "height": t.height,
        }),
        EntityType::MText(t) => json!({
            "type": "mtext", "handle": t.common.handle.value(),
            "layer": t.common.layer, "value": t.value,
            "position": [t.insertion_point.x, t.insertion_point.y],
            "height": t.height,
        }),
        _ => return None,
    };
    Some(v)
}


/// A block reference's placement, as a 2D transform.
///
/// Blocks are stored around their own base point; an `INSERT` places that
/// content with a scale and rotation. Applying it here means clients receive
/// ordinary world-space geometry and never have to know a block was involved.
#[derive(Clone, Copy)]
struct Placement {
    base: (f64, f64),
    offset: (f64, f64),
    scale: (f64, f64),
    cos_r: f64,
    sin_r: f64,
}

impl Placement {
    fn apply(&self, x: f64, y: f64) -> (f64, f64) {
        let sx = (x - self.base.0) * self.scale.0;
        let sy = (y - self.base.1) * self.scale.1;
        (
            self.offset.0 + sx * self.cos_r - sy * self.sin_r,
            self.offset.1 + sx * self.sin_r + sy * self.cos_r,
        )
    }

    /// Uniform scale factor for radii. Non-uniform scaling turns a circle into
    /// an ellipse, which our segment model cannot express, so such blocks are
    /// reported as skipped rather than silently distorted.
    fn uniform_scale(&self) -> Option<f64> {
        let (sx, sy) = self.scale;
        if (sx.abs() - sy.abs()).abs() <= 1e-9 * sx.abs().max(1.0) {
            Some(sx.abs())
        } else {
            None
        }
    }

    fn rotation(&self) -> f64 {
        self.sin_r.atan2(self.cos_r)
    }
}

/// Re-emit one entity through a block placement.
fn place_entity(e: &EntityType, p: &Placement) -> Option<Value> {
    let mut v = entity_json(e)?;
    let obj = v.as_object_mut()?;

    let map_pt = |val: &Value| -> Option<Value> {
        let a = val.as_array()?;
        let (x, y) = p.apply(a.first()?.as_f64()?, a.get(1)?.as_f64()?);
        Some(json!([x, y]))
    };

    for key in ["start", "end", "center", "position", "insert_point"] {
        if let Some(cur) = obj.get(key) {
            if let Some(moved) = map_pt(cur) {
                obj.insert(key.to_string(), moved);
            }
        }
    }
    if let Some(verts) = obj.get("vertices").and_then(Value::as_array).cloned() {
        let mut out = Vec::with_capacity(verts.len());
        for w in &verts {
            let a = w.as_array()?;
            let (x, y) = p.apply(a.first()?.as_f64()?, a.get(1)?.as_f64()?);
            // The bulge is scale- and rotation-invariant for uniform scaling.
            out.push(json!([x, y, a.get(2).and_then(Value::as_f64).unwrap_or(0.0)]));
        }
        obj.insert("vertices".into(), Value::Array(out));
    }
    if let Some(r) = obj.get("radius").and_then(Value::as_f64) {
        obj.insert("radius".into(), json!(r * p.uniform_scale()?));
    }
    for key in ["start_angle", "end_angle"] {
        if let Some(a) = obj.get(key).and_then(Value::as_f64) {
            obj.insert(key.to_string(), json!(a + p.rotation()));
        }
    }
    obj.insert("from_block".into(), json!(true));
    Some(v)
}

/// Expand a block reference into world-space entities.
///
/// Returns `None` when the block cannot be resolved or is scaled non-uniformly;
/// the caller then reports the insert as skipped instead of guessing.
fn expand_insert(
    doc: &acadrust::CadDocument,
    ins: &acadrust::entities::Insert,
    depth: usize,
    out: &mut Vec<Value>,
) -> Option<usize> {
    if depth >= 8 {
        return None;                       // guard against cyclic definitions
    }
    let record = doc
        .block_records
        .iter()
        .find(|br| br.name.eq_ignore_ascii_case(&ins.block_name))?;

    let rot = ins.rotation;
    let place = Placement {
        base: (record.base_point.x, record.base_point.y),
        offset: (ins.insert_point.x, ins.insert_point.y),
        scale: (ins.x_scale(), ins.y_scale()),
        cos_r: rot.cos(),
        sin_r: rot.sin(),
    };
    if place.uniform_scale().is_none() {
        return None;
    }

    let mut n = 0;
    for h in &record.entity_handles {
        let Some(child) = doc.get_entity(*h) else { continue };
        match child {
            EntityType::Insert(nested) => {
                n += expand_insert(doc, nested, depth + 1, out).unwrap_or(0);
            }
            other => {
                if let Some(v) = place_entity(other, &place) {
                    out.push(v);
                    n += 1;
                }
            }
        }
    }
    Some(n)
}

// ---------------------------------------------------------------------------
// geometry: JSON -> EntityType (writing)
// ---------------------------------------------------------------------------

fn f(v: &Value, k: &str) -> Result<f64, String> {
    v[k].as_f64().ok_or_else(|| format!("field {k:?} missing or not a number"))
}

fn pt(v: &Value, k: &str) -> Result<(f64, f64), String> {
    let a = v[k].as_array().ok_or_else(|| format!("field {k:?} missing"))?;
    match (a.first().and_then(Value::as_f64), a.get(1).and_then(Value::as_f64)) {
        (Some(x), Some(y)) => Ok((x, y)),
        _ => Err(format!("field {k:?} is not [x, y]")),
    }
}

fn entity_from_json(v: &Value) -> Result<EntityType, String> {
    let layer = v["layer"].as_str().unwrap_or("0").to_string();
    let t = v["type"].as_str().ok_or("field \"type\" missing")?;
    let e = match t {
        "line" => {
            let mut l = Line::new();
            let (x1, y1) = pt(v, "start")?;
            let (x2, y2) = pt(v, "end")?;
            l.start = Vector3::new(x1, y1, 0.0);
            l.end = Vector3::new(x2, y2, 0.0);
            l.common.layer = layer;
            EntityType::Line(l)
        }
        "arc" => {
            let mut a = CadArc::new();
            let (cx, cy) = pt(v, "center")?;
            a.center = Vector3::new(cx, cy, 0.0);
            a.radius = f(v, "radius")?;
            a.start_angle = f(v, "start_angle")?;
            a.end_angle = f(v, "end_angle")?;
            a.common.layer = layer;
            EntityType::Arc(a)
        }
        "circle" => {
            let mut c = Circle::new();
            let (cx, cy) = pt(v, "center")?;
            c.center = Vector3::new(cx, cy, 0.0);
            c.radius = f(v, "radius")?;
            c.common.layer = layer;
            EntityType::Circle(c)
        }
        "lwpolyline" => {
            let mut p = LwPolyline::new();
            let verts = v["vertices"].as_array().ok_or("field \"vertices\" missing")?;
            for w in verts {
                let a = w.as_array().ok_or("vertex is not [x, y, bulge]")?;
                let x = a.first().and_then(Value::as_f64).ok_or("vertex misses x")?;
                let y = a.get(1).and_then(Value::as_f64).ok_or("vertex misses y")?;
                let b = a.get(2).and_then(Value::as_f64).unwrap_or(0.0);
                let mut lv = LwVertex::new(Vector2::new(x, y));
                lv.bulge = b;
                p.vertices.push(lv);
            }
            p.is_closed = v["closed"].as_bool().unwrap_or(false);
            p.common.layer = layer;
            EntityType::LwPolyline(p)
        }
        other => return Err(format!("unknown entity type {other:?}")),
    };
    Ok(e)
}

// ---------------------------------------------------------------------------
// requests through the sender (click-free, from the socket thread)
// ---------------------------------------------------------------------------

/// Cache the host's thread-safe write channel, once.
///
/// `HostApi` is only handed to us during a dispatch, and only the host starts
/// dispatches - a plugin cannot call itself. But the host routes *every*
/// command through every loaded plugin (see `try_dispatch`), so the first
/// command the user runs - ours or not - is enough to connect. That makes the
/// Connect button a fallback rather than a requirement.
/// Remember which file the active tab holds. Added in API v5.
fn cache_document_path(host: &dyn HostApi) {
    let path = host.document_path(host.tab_id()).map(|p| p.display().to_string());
    if let Ok(mut g) = DOCUMENT_PATH.lock() {
        *g = path;
    }
}

fn cache_sender(host: &mut dyn HostApi) -> bool {
    if let Ok(g) = SENDER.lock() {
        if g.is_some() {
            return true;
        }
    }
    if let Some(s) = host.plugin_request_sender() {
        if let Ok(mut g) = SENDER.lock() {
            *g = Some(Arc::from(s));
            return true;
        }
    }
    false
}

fn send_req(req: PluginRequest) -> Result<PluginResponse, String> {
    let sender = {
        let guard = SENDER.lock().map_err(|_| "sender lock poisoned".to_string())?;
        guard
            .as_ref()
            .ok_or("not connected yet: run any command, or click Connect in the MCP Bridge tab")?
            .clone()
    };
    // Lock released: a blocked host call no longer blocks the whole bridge.
    sender.request(req).map_err(|e| format!("{e:?}"))
}

/// Selection + geometry, click-free through a document snapshot.
fn live_selection() -> Value {
    let handles = LAST_SELECTION.lock().map(|g| g.clone()).unwrap_or_default();
    let notif = NOTIF_SELECTION.load(Ordering::Relaxed);

    match send_req(PluginRequest::DocumentSnapshot) {
        Ok(PluginResponse::Document(doc)) => {
            let mut entities = Vec::new();
            let mut skipped = std::collections::BTreeMap::new();
            let mut expanded = 0usize;
            for raw in &handles {
                match doc.get_entity(acadrust::Handle::new(*raw)) {
                    // Block references carry no geometry of their own, so a
                    // client would see nothing usable. Expand them here, where
                    // the block definitions are, rather than making every
                    // caller explode blocks by hand in the GUI first.
                    Some(EntityType::Insert(ins)) => {
                        match expand_insert(&doc, ins, 0, &mut entities) {
                            Some(n) => expanded += n,
                            None => {
                                *skipped.entry("Insert (unresolved or non-uniform scale)".into())
                                    .or_insert(0usize) += 1;
                            }
                        }
                    }
                    Some(e) => match entity_json(e) {
                        Some(v) => entities.push(v),
                        None => *skipped
                            .entry(kind_name(e).to_string())
                            .or_insert(0usize) += 1,
                    },
                    None => *skipped.entry("not found".into()).or_insert(0) += 1,
                }
            }
            json!({
                "ok": true, "source": "snapshot (click-free)",
                "selected": handles.len(), "delivered": entities.len(),
                "from_blocks": expanded,
                "tab_id": SELECTION_TAB.lock().ok().and_then(|g| *g),
                // Straight from the live document, so callers no longer have
                // to guess or read the units out of a file on disk.
                "insertion_units": doc.header.insertion_units,
                "document_path": DOCUMENT_PATH.lock().ok().and_then(|g| g.clone()),
                "skipped_by_type": skipped,
                "unit": "drawing units", "angles": "radians",
                "notifications_selection": notif,
                "entities": entities,
            })
        }
        Ok(other) => json!({"ok": false,
            "error": format!("unexpected snapshot response: {other:?}")}),
        Err(msg) => {
            // No sender: fall back to what the Connect button captured.
            match SELECTION_JSON.lock().ok().and_then(|g| g.clone()) {
                Some(cached) => serde_json::from_str(&cached)
                    .unwrap_or_else(|_| json!({"ok": false, "error": "cache unreadable"})),
                None => json!({"ok": true, "selected": handles.len(),
                    "entities": Value::Null, "hint": msg,
                    "notifications_selection": notif}),
            }
        }
    }
}

/// Draw entities into the open drawing, as one undo step.
fn live_add(req: &Value) -> Value {
    let Some(items) = req["entities"].as_array() else {
        return json!({"ok": false, "error": "field \"entities\" missing"});
    };
    let mut entities = Vec::with_capacity(items.len());
    for (i, item) in items.iter().enumerate() {
        match entity_from_json(item) {
            Ok(e) => entities.push(e),
            Err(msg) => return json!({"ok": false,
                "error": format!("entity {i}: {msg}")}),
        }
    }
    if entities.is_empty() {
        return json!({"ok": false, "error": "nothing to draw"});
    }
    let label = req["undo_label"].as_str().unwrap_or("MCP Bridge").to_string();
    let n = entities.len();

    // Undo step first, so Ctrl+Z reverts the whole batch.
    if let Err(e) = send_req(PluginRequest::PushUndo { label }) {
        return json!({"ok": false, "error": e});
    }
    let handles = match send_req(PluginRequest::AddEntities(entities)) {
        Ok(PluginResponse::Handles(hs)) => {
            hs.iter().map(|h| h.value()).collect::<Vec<_>>()
        }
        Ok(other) => return json!({"ok": false,
            "error": format!("unexpected AddEntities response: {other:?}")}),
        Err(e) => return json!({"ok": false, "error": e}),
    };
    let _ = send_req(PluginRequest::SetDirty);
    let _ = send_req(PluginRequest::BumpGeometry);
    let _ = send_req(PluginRequest::PushInfo(format!(
        "MCP Bridge: drew {n} entities (Ctrl+Z reverts)"
    )));
    json!({"ok": true, "drawn": handles.len(), "handles": handles})
}

/// Delete entities, as one undo step.
fn live_remove(req: &Value) -> Value {
    let Some(items) = req["handles"].as_array() else {
        return json!({"ok": false, "error": "field \"handles\" missing"});
    };
    let handles: Vec<u64> = items.iter().filter_map(Value::as_u64).collect();
    if handles.is_empty() {
        return json!({"ok": false, "error": "no valid handles"});
    }
    let label = req["undo_label"].as_str().unwrap_or("MCP Bridge").to_string();
    if let Err(e) = send_req(PluginRequest::PushUndo { label }) {
        return json!({"ok": false, "error": e});
    }
    let mut removed = 0usize;
    for h in &handles {
        if let Ok(PluginResponse::Bool(true)) =
            send_req(PluginRequest::RemoveEntity { handle: acadrust::Handle::new(*h) })
        {
            removed += 1;
        }
    }
    let _ = send_req(PluginRequest::SetDirty);
    let _ = send_req(PluginRequest::BumpGeometry);
    json!({"ok": true, "removed": removed, "requested": handles.len()})
}

fn kind_name(e: &EntityType) -> &'static str {
    match e {
        EntityType::Point(_) => "Point",
        EntityType::Line(_) => "Line",
        EntityType::Circle(_) => "Circle",
        EntityType::Arc(_) => "Arc",
        EntityType::Ellipse(_) => "Ellipse",
        EntityType::Polyline(_) => "Polyline",
        EntityType::Polyline2D(_) => "Polyline2D",
        EntityType::Polyline3D(_) => "Polyline3D",
        EntityType::LwPolyline(_) => "LwPolyline",
        EntityType::Text(_) => "Text",
        EntityType::MText(_) => "MText",
        EntityType::Spline(_) => "Spline",
        EntityType::Dimension(_) => "Dimension",
        EntityType::Hatch(_) => "Hatch",
        EntityType::Solid(_) => "Solid",
        EntityType::Insert(_) => "Insert",
        _ => "other",
    }
}

// ---------------------------------------------------------------------------
// socket server
// ---------------------------------------------------------------------------

fn handle_request(line: &str) -> Value {
    let req: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(e) => return json!({"ok": false, "error": format!("invalid JSON: {e}")}),
    };
    match req["op"].as_str().unwrap_or("") {
        "status" => json!({
            "ok": true, "version": MANIFEST.version,
            "connected": SENDER.lock().map(|g| g.is_some()).unwrap_or(false),
            "selected": LAST_SELECTION.lock().map(|g| g.len()).unwrap_or(0),
            "tab_id": SELECTION_TAB.lock().ok().and_then(|g| *g),
            "notifications_total": NOTIF_TOTAL.load(Ordering::Relaxed),
            "notifications_selection": NOTIF_SELECTION.load(Ordering::Relaxed),
            // Host and plugin must be built by the same rustc: Rust has no
            // stable ABI and compound types cross the dll boundary. Compare
            // this against the `rustc/<hash>` string inside the host binary.
            "rustc": env!("BRIDGE_RUSTC"),
            "document_path": DOCUMENT_PATH.lock().ok().and_then(|g| g.clone()),
            "changes": latest_changes(),
            "issues": ISSUES_URL,
        }),
        "selection" => live_selection(),
        "add" => live_add(&req),
        "remove" => live_remove(&req),
        "info" => match req["msg"].as_str() {
            Some(m) => match send_req(PluginRequest::PushInfo(m.to_string())) {
                Ok(_) => json!({"ok": true}),
                Err(e) => json!({"ok": false, "error": e}),
            },
            None => json!({"ok": false, "error": "field \"msg\" missing"}),
        },
        other => json!({"ok": false, "error": format!("unknown op {other:?}")}),
    }
}

fn start_server() {
    SERVER.get_or_init(|| {
        std::thread::spawn(|| {
            let port = std::env::var("OCS_BRIDGE_PORT")
                .ok()
                .and_then(|p| p.parse::<u16>().ok())
                .unwrap_or(48810);
            let listener = match std::net::TcpListener::bind(("127.0.0.1", port)) {
                Ok(l) => l,
                Err(e) => {
                    let _ = std::fs::write(
                        std::env::temp_dir().join("ocs_mcp_bridge_socket_error.txt"),
                        format!("bind failed on {port}: {e}\n"),
                    );
                    return;
                }
            };
            let _ = std::fs::write(
                std::env::temp_dir().join("ocs_mcp_bridge_socket_ok.txt"),
                format!("listening on 127.0.0.1:{port}\n"),
            );
            for stream in listener.incoming() {
                let Ok(s) = stream else { continue };
                std::thread::spawn(move || {
                    use std::io::{BufRead, BufReader, Write};
                    let mut reader = BufReader::new(match s.try_clone() {
                        Ok(c) => c,
                        Err(_) => return,
                    });
                    let mut writer = s;
                    let mut line = String::new();
                    while let Ok(n) = reader.read_line(&mut line) {
                        if n == 0 {
                            break;
                        }
                        let resp = handle_request(line.trim());
                        if writer
                            .write_all(format!("{resp}\n").as_bytes())
                            .is_err()
                        {
                            break;
                        }
                        line.clear();
                    }
                });
            }
        });
    });
}

// ---------------------------------------------------------------------------
// plugin
// ---------------------------------------------------------------------------

struct BridgeModule {
    groups: OnceLock<Vec<RibbonGroup>>,
}

impl CadModule for BridgeModule {
    fn id(&self) -> &'static str {
        "mcp_bridge"
    }

    fn title(&self) -> &'static str {
        "MCP Bridge"
    }

    fn ribbon_groups(&self) -> &[RibbonGroup] {
        start_server();
        self.groups.get_or_init(|| {
            vec![RibbonGroup {
                title: "Bridge",
                tools: vec![
                    // Status first: it is the button you want when something
                    // looks wrong, and it prints the address to connect to.
                    RibbonItem::LargeTool(ToolDef {
                        id: "MCP_STATUS",
                        label: "Status",
                        icon: IconKind::Glyph("i"),
                        event: ModuleEvent::Command("MCP_STATUS".to_string()),
                    }),
                    RibbonItem::LargeTool(ToolDef {
                        id: "MCP_CONNECT",
                        label: "Connect",
                        icon: IconKind::Glyph("#"),
                        event: ModuleEvent::Command("MCP_CONNECT".to_string()),
                    }),
                    RibbonItem::LargeTool(ToolDef {
                        id: "MCP_ABOUT",
                        label: "About",
                        icon: IconKind::Glyph("?"),
                        event: ModuleEvent::Command("MCP_ABOUT".to_string()),
                    }),
                    RibbonItem::LargeTool(ToolDef {
                        id: "MCP_FEEDBACK",
                        label: "Feedback",
                        icon: IconKind::Glyph("!"),
                        event: ModuleEvent::Command("MCP_FEEDBACK".to_string()),
                    }),
                ],
            }]
        })
    }
}

struct BridgePlugin;

impl BuiltinPlugin for BridgePlugin {
    fn manifest(&self) -> &'static PluginManifest {
        &MANIFEST
    }

    fn ribbon(&self) -> Box<dyn CadModule> {
        Box::new(BridgeModule {
            groups: OnceLock::new(),
        })
    }

    fn dispatch(&self, host: &mut dyn HostApi, cmd: &str) -> bool {
        // Every command the user runs passes through here, ours or not, so the
        // first one connects the bridge without anyone clicking anything.
        cache_sender(host);
        cache_document_path(host);

        match cmd {
            "MCP_ABOUT" => {
                // The command line shows two lines at a time, so the panel
                // goes to the browser and only a pointer stays here.
                match about_html() {
                    Some(p) => {
                        open_url(&format!("file:///{}", p.display().to_string()
                            .replace('\\', "/")));
                        host.push_info(&format!(
                            "MCP Bridge {} - opened About in your browser.",
                            MANIFEST.version));
                        host.push_info(&format!("  {}", p.display()));
                    }
                    None => {
                        host.push_info(&format!("MCP Bridge {}", MANIFEST.version));
                        host.push_info(&format!("  rustc {}", env!("BRIDGE_RUSTC")));
                        host.push_error("Could not write the About page to the temp folder.");
                    }
                }
                true
            }
            "MCP_FEEDBACK" => {
                open_url(ISSUES_URL);
                host.push_info("MCP Bridge: opening the issue tracker in your browser.");
                host.push_info(&format!("  {ISSUES_URL}"));
                host.push_info(&format!(
                    "  Please mention version {} and rustc {}.",
                    MANIFEST.version,
                    env!("BRIDGE_RUSTC")
                ));
                true
            }
            "MCP_STATUS" => {
                start_server();
                let port = std::env::var("OCS_BRIDGE_PORT")
                    .ok()
                    .and_then(|p| p.parse::<u16>().ok())
                    .unwrap_or(48810);
                let connected = SENDER.lock().map(|g| g.is_some()).unwrap_or(false);
                let selected = LAST_SELECTION.lock().map(|g| g.len()).unwrap_or(0);
                let notif = NOTIF_SELECTION.load(Ordering::Relaxed);

                host.push_info(&format!("MCP Bridge {}", MANIFEST.version));
                host.push_info(&format!("  built by   rustc {}", env!("BRIDGE_RUSTC")));
                host.push_info(&format!("  address    127.0.0.1:{port}"));
                host.push_info(&format!(
                    "  channel    {}",
                    if connected {
                        "connected - reading and writing work"
                    } else {
                        "not connected - run any command to connect"
                    }
                ));
                host.push_info(&format!("  selection  {selected} entities"));
                host.push_info(&format!(
                    "  events     {notif} selection notifications{}",
                    if notif == 0 {
                        "  (needs host patch, see OpenCADStudio#879)"
                    } else {
                        ""
                    }
                ));
                true
            }
            "MCP_CONNECT" => {
                start_server();
                // `cache_sender` already ran at the top of dispatch, for this
                // and for every other command; this button only reports it.
                let connected = SENDER.lock().map(|g| g.is_some()).unwrap_or(false);

                // Fallback cache in case no sender is available.
                let handles =
                    LAST_SELECTION.lock().map(|g| g.clone()).unwrap_or_default();
                let doc = host.document();
                let mut entities = Vec::new();
                for raw in &handles {
                    if let Some(e) = doc.get_entity(acadrust::Handle::new(*raw)) {
                        if let Some(v) = entity_json(e) {
                            entities.push(v);
                        }
                    }
                }
                if let Ok(mut g) = SELECTION_JSON.lock() {
                    *g = Some(
                        json!({"ok": true, "source": "connect button",
                               "selected": handles.len(),
                               "delivered": entities.len(),
                               "unit": "drawing units",
                               "angles": "radians",
                               "entities": entities})
                        .to_string(),
                    );
                }
                host.push_info(&format!(
                    "MCP Bridge: {} - {} entities selected",
                    if connected {
                        "connected, click-free read/write enabled"
                    } else {
                        "no sender available, click-capture only"
                    },
                    handles.len()
                ));
                true
            }
            _ => false,
        }
    }

    // HostNotification is #[non_exhaustive]; `if let` keeps this compiling
    // when future host versions add variants.
    /// Set up before any user command runs (API v5).
    ///
    /// Until v5 a plugin could only reach `HostApi` from a dispatch, so a
    /// bridge with a worker thread had to wait for the user to happen to run
    /// something. This removes that dependency entirely.
    fn on_load(&mut self, host: &mut dyn HostApi) {
        start_server();
        cache_sender(host);
        cache_document_path(host);
    }

    fn on_notification(&mut self, _command_id: Option<u64>, notification: HostNotification) {
        NOTIF_TOTAL.fetch_add(1, Ordering::Relaxed);

        // Two shapes of the same event. 0.9.8 and later emit only
        // `SelectionChangedV4`, which carries the tab the selection belongs to;
        // the tab-less `SelectionChanged` is kept for hosts that predate it.
        // `HostNotification` is #[non_exhaustive], so the catch-all arm is
        // required and also keeps this compiling against future variants.
        let selection = match notification {
            HostNotification::SelectionChangedV4 { tab_id, handles } => {
                Some((Some(tab_id), handles))
            }
            HostNotification::SelectionChanged { handles } => Some((None, handles)),
            _ => None,
        };

        if let Some((tab_id, handles)) = selection {
            NOTIF_SELECTION.fetch_add(1, Ordering::Relaxed);
            if let Ok(mut g) = LAST_SELECTION.lock() {
                *g = handles.iter().map(|h| h.value()).collect();
            }
            if let Ok(mut g) = SELECTION_TAB.lock() {
                *g = tab_id;
            }
            // Any captured geometry belongs to the previous selection.
            if let Ok(mut g) = SELECTION_JSON.lock() {
                *g = None;
            }
        }
    }
}

ocs_plugin_api::export_plugin!(BridgePlugin);
