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
//!   {"op":"selection"}                   -> handles + geometry (click-free
//!                                           once the sender is available)
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
//! The Connect button is only a fallback. Live selection tracking
//! additionally requires the host to emit
//! `SelectionChanged`, which stock 0.9.7 does not do yet - see
//! <https://github.com/HakanSeven12/OpenCADStudio/issues/879>. Writing and
//! snapshot reads work on stock builds.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};

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
    version: "0.5.0",
    description: "Read/write bridge for MCP servers and external tools.",
    api_version: ApiVersion::CURRENT,
    ribbon_order: 60,
    xdata_apps: &[],
    command_prefixes: &["MCP_"],
};

/// Handles from the most recent `SelectionChanged`.
static LAST_SELECTION: Mutex<Vec<u64>> = Mutex::new(Vec::new());
/// Geometry captured via the Connect button (fallback without a sender).
static SELECTION_JSON: Mutex<Option<String>> = Mutex::new(None);
/// Thread-safe write channel to the host, cached on the first dispatch.
static SENDER: Mutex<Option<Box<dyn PluginRequestSender>>> = Mutex::new(None);

static NOTIF_TOTAL: AtomicUsize = AtomicUsize::new(0);
static NOTIF_SELECTION: AtomicUsize = AtomicUsize::new(0);
static SERVER: OnceLock<()> = OnceLock::new();

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
fn cache_sender(host: &mut dyn HostApi) -> bool {
    if let Ok(g) = SENDER.lock() {
        if g.is_some() {
            return true;
        }
    }
    if let Some(s) = host.plugin_request_sender() {
        if let Ok(mut g) = SENDER.lock() {
            *g = Some(s);
            return true;
        }
    }
    false
}

fn send_req(req: PluginRequest) -> Result<PluginResponse, String> {
    let guard = SENDER.lock().map_err(|_| "sender lock poisoned".to_string())?;
    let s = guard.as_ref().ok_or(
        "not connected yet: click Connect once in the MCP Bridge ribbon tab",
    )?;
    s.request(req).map_err(|e| format!("{e:?}"))
}

/// Selection + geometry, click-free through a document snapshot.
fn live_selection() -> Value {
    let handles = LAST_SELECTION.lock().map(|g| g.clone()).unwrap_or_default();
    let notif = NOTIF_SELECTION.load(Ordering::Relaxed);

    match send_req(PluginRequest::DocumentSnapshot) {
        Ok(PluginResponse::Document(doc)) => {
            let mut entities = Vec::new();
            let mut skipped = std::collections::BTreeMap::new();
            for raw in &handles {
                match doc.get_entity(acadrust::Handle::new(*raw)) {
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
            "notifications_total": NOTIF_TOTAL.load(Ordering::Relaxed),
            "notifications_selection": NOTIF_SELECTION.load(Ordering::Relaxed),
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

        match cmd {
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
                // The sender is the thread-safe write channel; once cached,
                // the socket thread can read and write click-free.
                let had = SENDER.lock().map(|g| g.is_some()).unwrap_or(false);
                if !had {
                    if let Some(s) = host.plugin_request_sender() {
                        if let Ok(mut g) = SENDER.lock() {
                            *g = Some(s);
                        }
                    }
                }
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
    fn on_notification(&mut self, _command_id: Option<u64>, notification: HostNotification) {
        NOTIF_TOTAL.fetch_add(1, Ordering::Relaxed);
        if let HostNotification::SelectionChanged { handles } = notification {
            NOTIF_SELECTION.fetch_add(1, Ordering::Relaxed);
            if let Ok(mut g) = LAST_SELECTION.lock() {
                *g = handles.iter().map(|h| h.value()).collect();
            }
            // Any captured geometry belongs to the previous selection.
            if let Ok(mut g) = SELECTION_JSON.lock() {
                *g = None;
            }
        }
    }
}

ocs_plugin_api::export_plugin!(BridgePlugin);
