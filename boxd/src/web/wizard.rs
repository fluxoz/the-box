//! The browser install wizard (Door 2, networked): a small public axum app
//! boxd serves *in the installer*, before any Box OS exists, so an operator on
//! the LAN can pick a disk layout with real disks in front of them and watch
//! the install run. It drives the exact same `box-core` pipeline as the console
//! TUI (probe → resolve → disko::render) and produces identical orders.
//!
//! No auth: this runs on a blank, unpaired machine and *is* the setup step.
//! Committing wipes disks, so it demands an explicit ERASE confirmation; the
//! machine has no data yet, and the window closes the moment the install
//! begins. (A console-shown setup PIN is a future hardening — see memory.)

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    response::Html,
    routing::{get, post},
    Json, Router,
};
use box_core::{disko, orders, probe, resolve, LayoutKind, ResolveOpts};
use maud::{html, PreEscaped, DOCTYPE};
use serde_json::{json, Value};

const CSS: &str = include_str!("style.css");

/// Where the wizard drops its results for the installer to act on, and where it
/// reads install progress back from.
#[derive(Clone)]
pub struct WizardCfg {
    pub orders_out: PathBuf,
    pub disko_out: PathBuf,
    /// Touched on commit — the installer waits on this (and the console wizard
    /// exits when it appears, so the two doors never both drive an install).
    pub commit_flag: PathBuf,
    /// The installer tees disko/nixos-install output here for /api/status.
    pub progress: PathBuf,
    /// Touched by the installer at BOX INSTALL COMPLETE.
    pub done: PathBuf,
    /// Base orders (name/wifi/key/pairing) when the operator deferred only the
    /// storage choice — used unless the browser pastes its own setup code.
    pub base_orders: Option<PathBuf>,
    /// Setup PIN shown on the box console; when set, /api/* require it. Guards
    /// the (destructive) setup window on an otherwise-open blank box.
    pub pin: Option<String>,
}

type St = State<Arc<WizardCfg>>;

/// `Some(403)` when a PIN is required and the request didn't present it.
fn pin_reject(cfg: &WizardCfg, headers: &HeaderMap) -> Option<(StatusCode, Json<Value>)> {
    let pin = cfg.pin.as_deref()?;
    let given = headers.get("x-setup-pin").and_then(|v| v.to_str().ok());
    if given == Some(pin) {
        None
    } else {
        Some((
            StatusCode::FORBIDDEN,
            Json(json!({ "ok": false, "error": "setup PIN required" })),
        ))
    }
}

/// The base config to merge a layout into: the browser's pasted code wins, else
/// the installer-provided base orders, else nothing.
fn base_config(cfg: &WizardCfg, body: &Value) -> Value {
    if let Some(v) = body.get("base") {
        if !v.is_null() {
            return v.clone();
        }
    }
    if let Some(path) = &cfg.base_orders {
        if let Ok(text) = std::fs::read_to_string(path) {
            if let Ok(v) = serde_json::from_str::<Value>(&text) {
                return v;
            }
        }
    }
    Value::Null
}

pub fn router(cfg: WizardCfg) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/api/probe", get(probe_disks))
        .route("/api/plan", post(plan))
        .route("/api/commit", post(commit))
        .route("/api/status", get(status))
        .with_state(Arc::new(cfg))
}

pub fn run(cfg: WizardCfg, listen: SocketAddr) -> anyhow::Result<()> {
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async move {
        let app = router(cfg);
        let listener = tokio::net::TcpListener::bind(listen).await?;
        tracing::info!("install wizard listening on http://{listen}");
        axum::serve(listener, app).await?;
        Ok::<_, anyhow::Error>(())
    })
}

// ---- endpoints ----------------------------------------------------------

async fn probe_disks(State(cfg): St, headers: HeaderMap) -> (StatusCode, Json<Value>) {
    if let Some(r) = pin_reject(&cfg, &headers) {
        return r;
    }
    let body = match probe::probe() {
        Ok(disks) => json!({ "ok": true, "disks": disks }),
        Err(e) => json!({ "ok": false, "error": e.to_string() }),
    };
    (StatusCode::OK, Json(body))
}

async fn plan(State(cfg): St, headers: HeaderMap, Json(body): Json<Value>) -> (StatusCode, Json<Value>) {
    if let Some(r) = pin_reject(&cfg, &headers) {
        return r;
    }
    let Some(kind) = body.get("layout").and_then(Value::as_str).and_then(LayoutKind::parse) else {
        return (StatusCode::OK, Json(json!({ "ok": false, "error": "unknown layout" })));
    };
    let disks = match probe::probe() {
        Ok(d) => d,
        Err(e) => return (StatusCode::OK, Json(json!({ "ok": false, "error": e.to_string() }))),
    };
    let body = match resolve::resolve(&disks, kind, &ResolveOpts::default()) {
        Ok(layout) => {
            let devices: Vec<Value> = layout
                .devices
                .iter()
                .map(|d| json!({ "path": d.path, "size_gb": d.size_gb(), "model": d.model, "kind": d.kind() }))
                .collect();
            json!({
                "ok": true,
                "layout": layout.kind.as_str(),
                "label": layout.kind.label(),
                "usable_gb": layout.usable_gb(),
                "filesystem": layout.filesystem,
                "raid": layout.raid,
                "devices": devices,
                "warnings": layout.warnings,
            })
        }
        Err(e) => json!({ "ok": false, "error": e.to_string() }),
    };
    (StatusCode::OK, Json(body))
}

async fn commit(State(cfg): St, headers: HeaderMap, Json(body): Json<Value>) -> (StatusCode, Json<Value>) {
    if let Some(r) = pin_reject(&cfg, &headers) {
        return r;
    }
    let Some(kind) = body.get("layout").and_then(Value::as_str).and_then(LayoutKind::parse) else {
        return (StatusCode::OK, Json(json!({ "ok": false, "error": "unknown layout" })));
    };
    let disks = match probe::probe() {
        Ok(d) => d,
        Err(e) => return (StatusCode::OK, Json(json!({ "ok": false, "error": e.to_string() }))),
    };
    let layout = match resolve::resolve(&disks, kind, &ResolveOpts::default()) {
        Ok(l) => l,
        Err(e) => return (StatusCode::OK, Json(json!({ "ok": false, "error": e.to_string() }))),
    };
    // The pasted setup code (browser, already decoded) wins; else the installer-
    // provided base orders; else null for a blank box configured entirely here.
    let base = base_config(&cfg, &body);
    let effective = orders::effective_orders(&base, &layout);

    if let Err(e) = write_commit(&cfg, &effective, &layout) {
        return (StatusCode::OK, Json(json!({ "ok": false, "error": format!("{e:#}") })));
    }
    let host = effective.get("hostname").and_then(Value::as_str).unwrap_or("box");
    (StatusCode::OK, Json(json!({ "ok": true, "hostname": host })))
}

fn write_commit(cfg: &WizardCfg, effective: &Value, layout: &box_core::ResolvedLayout) -> anyhow::Result<()> {
    std::fs::write(&cfg.orders_out, serde_json::to_string_pretty(effective)?)?;
    std::fs::write(&cfg.disko_out, disko::render(layout))?;
    // Commit flag last: only meaningful once the orders + disko are on disk.
    std::fs::write(&cfg.commit_flag, b"1")?;
    Ok(())
}

async fn status(State(cfg): St, headers: HeaderMap) -> (StatusCode, Json<Value>) {
    if let Some(r) = pin_reject(&cfg, &headers) {
        return r;
    }
    let committed = cfg.commit_flag.exists();
    let done = cfg.done.exists();
    let log = tail(&cfg.progress, 6000);
    (StatusCode::OK, Json(json!({ "committed": committed, "done": done, "log": log })))
}

/// Last `max` bytes of a file (best-effort), trimmed to whole lines.
fn tail(path: &std::path::Path, max: usize) -> String {
    let Ok(data) = std::fs::read(path) else {
        return String::new();
    };
    let start = data.len().saturating_sub(max);
    let slice = &data[start..];
    let text = String::from_utf8_lossy(slice);
    if start > 0 {
        // drop a partial first line
        match text.find('\n') {
            Some(i) => text[i + 1..].to_string(),
            None => text.to_string(),
        }
    } else {
        text.to_string()
    }
}

// ---- page ---------------------------------------------------------------

async fn index() -> Html<String> {
    let markup = html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                title { "Set up storage · The Box" }
                style { (PreEscaped(CSS)) }
                style { (PreEscaped(WIZ_CSS)) }
            }
            body {
                header.top {
                    span.brand { span.mark { "THE " b { "BOX" } } span.sub { "Set up storage" } }
                }
                main.wiz {
                    section.card #pincard {
                        h2 { "Setup PIN" }
                        p.muted { "Enter the PIN shown on this box's screen, then Continue. If no PIN is shown, just click Continue." }
                        div.eraserow {
                            input #pin type="text" autocomplete="off" spellcheck="false" placeholder="PIN";
                            button #pincont type="button" { "Continue" }
                        }
                        div #pinnote .codenote {}
                    }
                    section.card {
                        h2 { "1 · This machine's disks" }
                        div #disks .disks { p.muted { "Enter the setup PIN above to begin." } }
                    }
                    section.card {
                        h2 { "2 · Choose a layout" }
                        div.seg role="radiogroup" {
                            label { input type="radio" name="layout" value="single" checked; span { "Single" } small { "one disk" } }
                            label { input type="radio" name="layout" value="mirror"; span { "Mirror" } small { "survives one disk failing" } }
                            label { input type="radio" name="layout" value="pool"; span { "Pool" } small { "one big volume, no safety net" } }
                        }
                        div #verdict .verdict { p.muted { "…" } }
                    }
                    section.card {
                        h2 { "3 · Confirm" }
                        details.handoff {
                            summary { "Have a setup code from the Configurator? Paste it" }
                            p.muted { "Carries this box's name, Wi-Fi, your SSH key and pairing code. Optional — without it the box is set up local-only and paired at the console." }
                            textarea #code placeholder="paste setup code (optional)" spellcheck="false" {}
                            div #codenote .codenote {}
                        }
                        p.danger { "This ERASES the disks above and installs Box OS. All data on them is lost." }
                        div.eraserow {
                            label for="erase" { "Type " b { "ERASE" } " to proceed:" }
                            input #erase type="text" autocomplete="off" spellcheck="false" placeholder="ERASE";
                            button #go type="button" disabled { "Install Box OS" }
                        }
                    }
                    section #progress .card .hidden {
                        h2 #progress-title { "Installing…" }
                        pre #log .log {}
                    }
                }
                footer.wiz-footer { "THE BOX · installer · pre-pairing setup" }
                script { (PreEscaped(WIZ_JS)) }
            }
        }
    };
    Html(markup.into_string())
}

const WIZ_CSS: &str = r#"
.brand{display:flex;align-items:baseline;gap:.6rem}
.brand .mark{font-weight:800;letter-spacing:.12em}
.brand .sub{color:var(--dim);text-transform:uppercase;letter-spacing:.18em;font-size:.72rem}
main.wiz{max-width:760px;margin:1.4rem auto;padding:0 1rem;display:flex;flex-direction:column;gap:1rem}
.card{background:var(--panel);border:1px solid var(--line);border-radius:8px;padding:1rem 1.1rem;box-shadow:0 1px 0 var(--shadow)}
.card h2{margin:.1rem 0 .8rem;font-size:.95rem;letter-spacing:.02em;color:var(--brass-bright)}
.muted{color:var(--dim)}
.disks{display:flex;flex-direction:column;gap:.35rem;font-family:var(--mono);font-size:.86rem}
.disk{display:flex;gap:.6rem;align-items:center;padding:.35rem .5rem;border:1px solid var(--line);border-radius:6px;background:var(--panel-2)}
.disk.pick{border-color:var(--brass);box-shadow:inset 3px 0 0 var(--brass)}
.disk.rm{opacity:.5}
.disk .tag{margin-left:auto;color:var(--faint);font-size:.76rem;text-transform:uppercase;letter-spacing:.08em}
.seg{display:grid;grid-template-columns:1fr 1fr 1fr;gap:.5rem;margin-bottom:.8rem}
.seg label{display:flex;flex-direction:column;gap:.15rem;border:1px solid var(--line);border-radius:6px;padding:.5rem .6rem;cursor:pointer;background:var(--panel-2)}
.seg label:has(input:checked){border-color:var(--brass);background:var(--brass-wash)}
.seg input{position:absolute;opacity:0;width:0;height:0}
.seg span{font-weight:600}
.seg small{color:var(--dim);font-size:.72rem}
.verdict{border-top:1px dashed var(--line);padding-top:.7rem;font-size:.9rem}
.verdict .cap{color:var(--ok);font-weight:700}
.verdict ul{margin:.4rem 0 0;padding-left:1.1rem}
.verdict .warn{color:var(--critical)}
.verdict.bad{color:var(--critical)}
.handoff{margin-bottom:.8rem}
.handoff summary{cursor:pointer;color:var(--brass-bright)}
.handoff textarea{width:100%;min-height:3.4rem;margin-top:.5rem;font-family:var(--mono);font-size:.8rem;background:var(--panel-2);color:var(--ink);border:1px solid var(--line);border-radius:6px;padding:.5rem;resize:vertical}
.codenote{font-size:.8rem;margin-top:.3rem}
.codenote.ok{color:var(--ok)} .codenote.bad{color:var(--critical)}
.danger{color:var(--critical);font-weight:600}
.eraserow{display:flex;gap:.6rem;align-items:center;flex-wrap:wrap}
.eraserow input{font-family:var(--mono);text-transform:uppercase;letter-spacing:.2em;background:var(--panel-2);color:var(--ink);border:1px solid var(--line-strong);border-radius:6px;padding:.45rem .6rem;width:9rem}
#go{background:var(--brass);color:#fff;border:none;border-radius:6px;padding:.5rem 1rem;font-weight:700;cursor:pointer}
#go:disabled{opacity:.4;cursor:not-allowed}
.log{background:#0c0a06;color:#d6c9a8;border-radius:6px;padding:.7rem;max-height:22rem;overflow:auto;font-size:.78rem;white-space:pre-wrap;word-break:break-word}
.hidden{display:none}
.wiz-footer{max-width:760px;margin:0 auto 2rem;padding:0 1rem;color:var(--faint);font-size:.75rem;letter-spacing:.06em;text-transform:uppercase}
"#;

const WIZ_JS: &str = r#"
const $=s=>document.querySelector(s);
const layout=()=>document.querySelector('input[name=layout]:checked').value;
const pin=()=>$('#pin').value.trim();
const H=()=>({'content-type':'application/json','X-Setup-Pin':pin()});
const HG=()=>({'X-Setup-Pin':pin()});
let planOk=false;

async function loadDisks(){
  const el=$('#disks'); const note=$('#pinnote');
  try{
    const res=await fetch('/api/probe',{headers:HG()});
    if(res.status===403){note.className='codenote bad';note.textContent='Setup PIN required or incorrect.';el.innerHTML='<p class="muted">Enter the setup PIN above to begin.</p>';return;}
    note.className='codenote';note.textContent='';
    const r=await res.json();
    if(!r.ok){el.innerHTML='<p class="warn">'+r.error+'</p>';return;}
    if(!r.disks.length){el.innerHTML='<p class="muted">No disks detected.</p>';return;}
    el.innerHTML=r.disks.map(d=>{
      const gb=Math.round(d.size_bytes/1e9);
      const tooSmall=d.size_bytes<8e9;
      const inelig=d.removable||tooSmall;
      const cls='disk'+(inelig?' rm':'');
      const tag=d.removable?'removable — never touched':(tooSmall?'too small — ignored':(d.rotational?'HDD':'SSD'));
      return `<div class="${cls}" data-name="${d.name}"><span>${d.path}</span><span>${gb} GB</span><span>${(d.model||'disk')}</span><span class="tag">${tag}</span></div>`;
    }).join('');
    plan();
  }catch(e){$('#disks').innerHTML='<p class="warn">'+e+'</p>';}
}

async function plan(){
  const v=$('#verdict');
  try{
    const r=await (await fetch('/api/plan',{method:'POST',headers:H(),body:JSON.stringify({layout:layout()})})).json();
    document.querySelectorAll('.disk').forEach(d=>d.classList.remove('pick'));
    if(!r.ok){planOk=false;v.className='verdict bad';v.innerHTML='<b>Not possible on this machine:</b><br>'+r.error;sync();return;}
    planOk=true;v.className='verdict';
    const names=r.devices.map(d=>d.path);
    document.querySelectorAll('.disk').forEach(d=>{ if(names.includes('/dev/'+d.dataset.name)||names.includes(d.querySelector('span').textContent)) d.classList.add('pick'); });
    let h=`<div><span class="cap">~${r.usable_gb} GB usable</span> · ${r.label} (${r.filesystem}${r.raid?', '+r.raid:''})</div>`;
    h+='<div class="muted">Will erase and use:</div><ul>'+r.devices.map(d=>`<li>${d.path} — ${d.size_gb} GB ${d.kind}</li>`).join('')+'</ul>';
    if(r.warnings&&r.warnings.length) h+=r.warnings.map(w=>`<div class="warn">! ${w}</div>`).join('');
    v.innerHTML=h;
  }catch(e){planOk=false;v.className='verdict bad';v.textContent=''+e;}
  sync();
}

function pendingBase(){
  const raw=$('#code').value.trim();
  const note=$('#codenote');
  if(!raw){note.className='codenote';note.textContent='';return {ok:true,base:null};}
  try{
    const base=JSON.parse(atob(raw));
    const name=base.hostname&&base.hostname!=='auto'?base.hostname:'this box';
    const bits=[]; if(base.ssh_authorized_keys&&base.ssh_authorized_keys.length)bits.push('SSH key'); if(base.wifi&&base.wifi.ssid)bits.push('Wi-Fi'); if(base.enrollment_code_hash)bits.push('pairing');
    note.className='codenote ok';note.textContent='Setup code for '+name+(bits.length?' · carries '+bits.join(', '):'');
    return {ok:true,base};
  }catch(e){note.className='codenote bad';note.textContent='That does not look like a valid setup code.';return {ok:false};}
}

function sync(){
  const erased=$('#erase').value.trim().toUpperCase()==='ERASE';
  const code=pendingBase();
  $('#go').disabled=!(planOk&&erased&&code.ok);
}

async function go(){
  const code=pendingBase(); if(!code.ok)return;
  $('#go').disabled=true;
  const r=await (await fetch('/api/commit',{method:'POST',headers:H(),body:JSON.stringify({layout:layout(),base:code.base})})).json();
  if(!r.ok){$('#verdict').className='verdict bad';$('#verdict').innerHTML='<b>Could not start:</b><br>'+r.error;$('#go').disabled=false;return;}
  document.querySelectorAll('.card').forEach(c=>{ if(c.id!=='progress')c.classList.add('hidden'); });
  $('#progress').classList.remove('hidden');
  $('#progress-title').textContent='Installing Box OS onto '+r.hostname+'…';
  poll();
}

async function poll(){
  try{
    const s=await (await fetch('/api/status',{headers:HG()})).json();
    if(s.log)$('#log').textContent=s.log;
    $('#log').scrollTop=$('#log').scrollHeight;
    if(s.done){$('#progress-title').textContent='Installed — the box is rebooting into Box OS.';return;}
  }catch(e){}
  setTimeout(poll,2000);
}

document.querySelectorAll('input[name=layout]').forEach(r=>r.addEventListener('change',plan));
$('#erase').addEventListener('input',sync);
$('#code').addEventListener('input',sync);
$('#go').addEventListener('click',go);
$('#pincont').addEventListener('click',loadDisks);
$('#pin').addEventListener('keydown',e=>{ if(e.key==='Enter')loadDisks(); });
loadDisks();
"#;
