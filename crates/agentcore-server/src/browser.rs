//! A headless Chromium that agentcore drives over the DevTools protocol, so
//! an agent can show a reviewer the delivered change on its preview.
//!
//! The browser runs on the agentcore server, not in the agent's sandbox (the
//! sandbox has no network). Every request it makes is checked here first:
//! pages may only be opened on the preview (and hosts an admin listed for
//! the project), nothing may reach private or local addresses other than
//! the preview itself, and the preview's protection bypass secret is only
//! sent to the preview.

use std::collections::HashMap;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agentcore_core::ReviewDevice;
use base64::Engine;
use futures::{SinkExt, StreamExt};
use reqwest::Url;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::tungstenite::Message;

const COMMAND_TIMEOUT: Duration = Duration::from_secs(30);
const SETTLE_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_ELEMENTS: usize = 150;

const PHONE_UA: &str = "Mozilla/5.0 (Linux; Android 14; Pixel 8) AppleWebKit/537.36 \
                        (KHTML, like Gecko) Chrome/130.0.0.0 Mobile Safari/537.36";
const DESKTOP_UA: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 \
                          (KHTML, like Gecko) Chrome/130.0.0.0 Safari/537.36";

/// Where a Chromium may be installed, tried in order after the configured
/// path and `AGENTCORE_CHROMIUM`.
const CANDIDATES: &[&str] = &[
    "chromium",
    "chromium-browser",
    "google-chrome",
    "google-chrome-stable",
    "chrome-headless-shell",
    "chrome",
    "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
    "/Applications/Chromium.app/Contents/MacOS/Chromium",
];

/// Whether Chromium may use its own sandbox: not when the configuration or
/// `AGENTCORE_CHROMIUM_NO_SANDBOX` says so (hosts without user namespaces).
pub fn sandbox_allowed(configured: bool) -> bool {
    configured && std::env::var_os("AGENTCORE_CHROMIUM_NO_SANDBOX").is_none()
}

/// The Chromium to use: the configured one, `AGENTCORE_CHROMIUM`, or the
/// first one found on the `PATH` or in the usual places.
pub fn find_chromium(configured: Option<&Path>) -> Option<PathBuf> {
    if let Some(path) = configured {
        return path.is_file().then(|| path.to_path_buf());
    }
    if let Some(path) = std::env::var_os("AGENTCORE_CHROMIUM") {
        let path = PathBuf::from(path);
        return path.is_file().then_some(path);
    }
    let dirs: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default();
    CANDIDATES.iter().find_map(|c| {
        let c = Path::new(c);
        if c.is_absolute() {
            return c.is_file().then(|| c.to_path_buf());
        }
        dirs.iter().map(|d| d.join(c)).find(|p| p.is_file())
    })
}

// ---- what the browser may reach ----------------------------------------------------

/// What the review's browser may open and where the preview's secret goes.
#[derive(Debug, Clone)]
pub struct Access {
    /// The preview: `scheme://host[:port]`.
    pub preview: Url,
    /// Other hosts an admin allowed for this project (e.g. a sign-in page);
    /// `*.example.com` matches its subdomains.
    pub extra_hosts: Vec<String>,
    /// Vercel's "Protection Bypass for Automation" secret, sent only to the
    /// preview.
    pub bypass_secret: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Route {
    /// The preview itself: allowed, with the bypass secret.
    Preview,
    /// A host the project allows: allowed as it is.
    Listed,
    /// Somewhere else on the internet: allowed for what a page loads (not
    /// as the page itself), once its address is known to be public.
    Elsewhere {
        host: String,
        port: u16,
    },
    Blocked(String),
}

fn same_origin(a: &Url, b: &Url) -> bool {
    a.scheme() == b.scheme()
        && a.host_str() == b.host_str()
        && a.port_or_known_default() == b.port_or_known_default()
}

fn host_listed(host: &str, port: Option<u16>, listed: &[String]) -> bool {
    let with_port = port.map(|p| format!("{host}:{p}"));
    listed.iter().any(|l| {
        if let Some(suffix) = l.strip_prefix("*.") {
            host.ends_with(&format!(".{suffix}"))
        } else {
            l == host || Some(l) == with_port.as_ref()
        }
    })
}

impl Access {
    /// Whether a page may be opened at `url` (top-level navigation).
    pub fn may_open(&self, url: &Url) -> bool {
        matches!(self.route(url, true), Route::Preview | Route::Listed)
    }

    fn route(&self, url: &Url, page: bool) -> Route {
        if !matches!(url.scheme(), "http" | "https") {
            return Route::Blocked(format!("{} addresses are not opened", url.scheme()));
        }
        if same_origin(url, &self.preview) {
            return Route::Preview;
        }
        let Some(host) = url
            .host_str()
            .map(|h| h.trim_matches(['[', ']']).to_string())
        else {
            return Route::Blocked("no host".into());
        };
        if host_listed(&host, url.port(), &self.extra_hosts) {
            return Route::Listed;
        }
        if page {
            return Route::Blocked(format!(
                "{} is not the preview: the review only opens pages on the preview \
                 (an admin can allow more sites in the project's preview access)",
                url.origin().ascii_serialization()
            ));
        }
        Route::Elsewhere {
            host,
            port: url.port_or_known_default().unwrap_or(443),
        }
    }
}

/// Addresses the browser must not reach unless they are the preview:
/// loopback, private, link-local (cloud metadata), shared and reserved
/// ranges.
fn is_internal(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let [a, b, ..] = v4.octets();
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
                || v4.is_multicast()
                || v4.is_documentation()
                || a == 0
                || (a == 100 && (64..128).contains(&b))
                || (a == 198 && (18..20).contains(&b))
                || a >= 240
        }
        IpAddr::V6(v6) => {
            let first = v6.segments()[0];
            v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || (first & 0xfe00) == 0xfc00
                || (first & 0xffc0) == 0xfe80
                || v6
                    .to_ipv4_mapped()
                    .is_some_and(|v4| is_internal(IpAddr::V4(v4)))
        }
    }
}

/// Resolve a host and refuse it if any of its addresses is internal.
async fn public_host(host: &str, port: u16) -> Result<(), String> {
    if let Ok(ip) = host.parse::<IpAddr>() {
        return if is_internal(ip) {
            Err(format!("{host} is a private or local address"))
        } else {
            Ok(())
        };
    }
    let addrs: Vec<_> = tokio::net::lookup_host((host, port))
        .await
        .map_err(|e| format!("{host}: {e}"))?
        .collect();
    if addrs.is_empty() {
        return Err(format!("{host} has no address"));
    }
    match addrs.iter().find(|a| is_internal(a.ip())) {
        Some(a) => Err(format!("{host} resolves to the private address {}", a.ip())),
        None => Ok(()),
    }
}

// ---- the DevTools connection -------------------------------------------------------

type Reply = oneshot::Sender<Result<Value, String>>;

struct Cdp {
    out: mpsc::UnboundedSender<String>,
    next: AtomicU64,
    pending: Mutex<HashMap<u64, Reply>>,
}

impl Cdp {
    async fn send(
        &self,
        session: Option<&str>,
        method: &str,
        params: Value,
    ) -> Result<Value, String> {
        let id = self.next.fetch_add(1, Ordering::SeqCst);
        let mut msg = json!({ "id": id, "method": method, "params": params });
        if let Some(s) = session {
            msg["sessionId"] = json!(s);
        }
        let (tx, rx) = oneshot::channel();
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id, tx);
        if self.out.send(msg.to_string()).is_err() {
            return Err("the browser has closed".into());
        }
        match tokio::time::timeout(COMMAND_TIMEOUT, rx).await {
            Ok(Ok(reply)) => reply,
            Ok(Err(_)) => Err("the browser has closed".into()),
            Err(_) => {
                self.pending
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&id);
                Err(format!("the browser did not answer `{method}` in time"))
            }
        }
    }

    fn fire(&self, session: &str, method: &str, params: Value) {
        let id = self.next.fetch_add(1, Ordering::SeqCst);
        let msg = json!({ "id": id, "method": method, "params": params, "sessionId": session });
        let _ = self.out.send(msg.to_string());
    }
}

/// What the request filter needs, shared with the connection's reader.
struct Guard {
    access: Access,
    /// The page's main frame: its document requests are page navigations.
    main_frame: Mutex<String>,
    page_session: Mutex<String>,
    /// Hosts already found to be public (or not), with the reason.
    checked: tokio::sync::Mutex<HashMap<String, Result<(), String>>>,
    /// Things the agent should hear about (blocked requests, dialogs).
    notes: Mutex<Vec<String>>,
    /// A page navigation away from the preview was blocked: the browser
    /// shows an error page and goes back.
    left: std::sync::atomic::AtomicBool,
}

impl Guard {
    fn note(&self, text: String) {
        let mut notes = self.notes.lock().unwrap_or_else(|e| e.into_inner());
        if notes.len() < 20 && !notes.contains(&text) {
            notes.push(text);
        }
    }

    async fn decide(&self, cdp: &Cdp, session: &str, params: &Value) {
        let id = params["requestId"].as_str().unwrap_or_default().to_string();
        let raw = params["request"]["url"].as_str().unwrap_or_default();
        let page = params["resourceType"] == "Document"
            && session == *self.page_session.lock().unwrap_or_else(|e| e.into_inner())
            && params["frameId"].as_str()
                == Some(
                    self.main_frame
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .as_str(),
                );
        let route = match Url::parse(raw) {
            Ok(url) => self.access.route(&url, page),
            Err(_) => Route::Blocked("not a valid address".into()),
        };
        let blocked = match route {
            Route::Preview => {
                let mut headers: Vec<Value> = params["request"]["headers"]
                    .as_object()
                    .map(|h| {
                        h.iter()
                            .map(|(k, v)| json!({ "name": k, "value": v.as_str().unwrap_or_default() }))
                            .collect()
                    })
                    .unwrap_or_default();
                let mut args = json!({ "requestId": id });
                if let Some(secret) = &self.access.bypass_secret {
                    headers.push(json!({ "name": "x-vercel-protection-bypass", "value": secret }));
                    args["headers"] = json!(headers);
                }
                cdp.fire(session, "Fetch.continueRequest", args);
                return;
            }
            Route::Listed => None,
            Route::Elsewhere { host, port } => {
                let mut checked = self.checked.lock().await;
                let verdict = match checked.get(&host) {
                    Some(v) => v.clone(),
                    None => {
                        let v = public_host(&host, port).await;
                        checked.insert(host.clone(), v.clone());
                        v
                    }
                };
                verdict.err()
            }
            Route::Blocked(reason) => Some(reason),
        };
        match blocked {
            None => cdp.fire(session, "Fetch.continueRequest", json!({ "requestId": id })),
            Some(reason) => {
                let shown: String = raw.chars().take(120).collect();
                self.note(format!("Blocked {shown}: {reason}"));
                if page {
                    self.left.store(true, Ordering::SeqCst);
                }
                cdp.fire(
                    session,
                    "Fetch.failRequest",
                    json!({ "requestId": id, "errorReason": "BlockedByClient" }),
                );
            }
        }
    }
}

/// Start intercepting requests in a newly attached target (an iframe or a
/// worker), then let it run.
async fn watch_target(cdp: &Cdp, session: &str) {
    let _ = cdp
        .send(
            Some(session),
            "Fetch.enable",
            json!({ "patterns": [{ "urlPattern": "*" }] }),
        )
        .await;
    let _ = cdp
        .send(
            Some(session),
            "Target.setAutoAttach",
            json!({ "autoAttach": true, "waitForDebuggerOnStart": true, "flatten": true }),
        )
        .await;
    let _ = cdp
        .send(Some(session), "Runtime.runIfWaitingForDebugger", json!({}))
        .await;
}

async fn handle_event(cdp: Arc<Cdp>, guard: Arc<Guard>, msg: Value) {
    let session = msg["sessionId"].as_str().unwrap_or_default().to_string();
    let params = &msg["params"];
    match msg["method"].as_str().unwrap_or_default() {
        "Fetch.requestPaused" => guard.decide(&cdp, &session, params).await,
        "Page.javascriptDialogOpening" => {
            let text: String = params["message"]
                .as_str()
                .unwrap_or_default()
                .chars()
                .take(200)
                .collect();
            guard.note(format!(
                "The page showed a dialog and it was accepted: {text}"
            ));
            cdp.fire(
                &session,
                "Page.handleJavaScriptDialog",
                json!({ "accept": true }),
            );
        }
        // Our own attach (no parent session) is not a new target.
        "Target.attachedToTarget" if !session.is_empty() => {
            let child = params["sessionId"].as_str().unwrap_or_default().to_string();
            if params["targetInfo"]["type"] == "page" {
                // New windows and tabs are not followed: links open in place.
                guard.note("The page tried to open a new window; it was closed.".into());
                let target = params["targetInfo"]["targetId"].clone();
                let _ = cdp
                    .send(None, "Target.closeTarget", json!({ "targetId": target }))
                    .await;
            } else {
                watch_target(&cdp, &child).await;
            }
        }
        _ => {}
    }
}

// ---- the page ----------------------------------------------------------------------

/// What the agent reads about the page after each step.
#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    pub url: String,
    pub title: String,
    pub text: String,
    pub elements: Vec<String>,
}

impl Snapshot {
    pub fn render(&self, notes: &[String]) -> String {
        let mut out = format!("Page: {}\nAddress: {}\n", self.title, self.url);
        for n in notes {
            out.push_str(&format!("Note: {n}\n"));
        }
        out.push_str("\nThings you can use (pass the number as `ref`):\n");
        if self.elements.is_empty() {
            out.push_str("(none)\n");
        }
        for e in &self.elements {
            out.push_str(e);
            out.push('\n');
        }
        out.push_str("\nText on the page:\n");
        out.push_str(&self.text);
        out
    }
}

/// One browser with one page, for one review.
pub struct Page {
    cdp: Arc<Cdp>,
    guard: Arc<Guard>,
    session: String,
    child: tokio::sync::Mutex<Child>,
    _profile: tempfile::TempDir,
}

impl Drop for Page {
    fn drop(&mut self) {
        if let Ok(mut child) = self.child.try_lock() {
            let _ = child.start_kill();
        }
    }
}

/// Lists the page's visible controls, numbering each (`data-agentcore-ref`),
/// with its visible text. Password values are never read.
const SNAPSHOT_JS: &str = r#"(() => {
  const visible = (el) => { const r = el.getBoundingClientRect(); const s = getComputedStyle(el);
    return r.width > 0 && r.height > 0 && s.visibility !== 'hidden' && s.display !== 'none'; };
  document.querySelectorAll('[data-agentcore-ref]').forEach((e) => e.removeAttribute('data-agentcore-ref'));
  const sel = 'a[href],button,input:not([type=hidden]),select,textarea,summary,[role=button],[role=link],' +
    '[role=checkbox],[role=radio],[role=switch],[role=tab],[role=menuitem],[role=option],[role=combobox],' +
    '[contenteditable=""],[contenteditable=true]';
  const clean = (s) => String(s || '').trim().replace(/\s+/g, ' ');
  const items = []; let n = 0;
  for (const el of document.querySelectorAll(sel)) {
    if (n >= MAX || !visible(el)) continue;
    n += 1; el.setAttribute('data-agentcore-ref', String(n));
    const tag = el.tagName.toLowerCase(); const type = (el.getAttribute('type') || '').toLowerCase();
    let role = el.getAttribute('role');
    if (!role) role = tag === 'a' ? 'link' : tag === 'select' ? 'select' : tag === 'textarea' ? 'textbox'
      : tag === 'input' ? (['checkbox', 'radio'].includes(type) ? type : ['submit', 'button', 'reset'].includes(type) ? 'button' : 'textbox') : tag;
    const label = el.getAttribute('aria-label') || (el.labels && el.labels[0] && el.labels[0].innerText)
      || (tag !== 'select' && el.innerText) || (['submit', 'button'].includes(type) && el.value)
      || el.getAttribute('placeholder') || el.getAttribute('title') || el.getAttribute('alt') || el.getAttribute('name');
    let extra = '';
    if (tag === 'input' || tag === 'textarea') {
      if (type === 'password') extra = el.value ? ' (filled)' : ' (password)';
      else if (type === 'checkbox' || type === 'radio') extra = el.checked ? ' (checked)' : ' (not checked)';
      else if (el.value && role === 'textbox') extra = ' value=' + JSON.stringify(String(el.value).slice(0, 60));
      if (type && role === 'textbox' && type !== 'text' && type !== 'password') extra += ' type=' + type;
    }
    if (tag === 'select') extra = ' selected=' + JSON.stringify(clean(el.selectedOptions[0] && el.selectedOptions[0].text))
      + ' options: ' + Array.from(el.options).slice(0, 15).map((o) => JSON.stringify(clean(o.text))).join(', ');
    if (tag === 'a') { const h = el.getAttribute('href'); if (h && !h.startsWith('javascript:')) extra += ' -> ' + h.slice(0, 80); }
    if (el.disabled || el.getAttribute('aria-disabled') === 'true') extra += ' (disabled)';
    items.push('[' + n + '] ' + role + ' ' + JSON.stringify(clean(label).slice(0, 80)) + extra);
  }
  const text = clean((document.body && document.body.innerText || '').replace(/\n+/g, ' ¶ ')).replace(/ ¶ /g, '\n');
  return JSON.stringify({ title: document.title, url: location.href,
    text: text.length > 4000 ? text.slice(0, 4000) + '\n[…]' : text, elements: items });
})()"#;

/// Waits until the page stops changing (400 ms without DOM changes, at most
/// 4 s).
const SETTLE_JS: &str = r#"new Promise((done) => {
  let timer; const finish = () => { obs.disconnect(); done(true); };
  const obs = new MutationObserver(() => { clearTimeout(timer); timer = setTimeout(finish, 400); });
  obs.observe(document, { subtree: true, childList: true, attributes: true, characterData: true });
  timer = setTimeout(finish, 400); setTimeout(finish, 4000);
})"#;

/// Links and `window.open` stay in this page: the review follows one page.
const SAME_PAGE_JS: &str = r#"(() => {
  window.open = (url) => { if (url) location.href = url; return null; };
  document.addEventListener('click', (e) => {
    const a = e.target && e.target.closest && e.target.closest('a[target]');
    if (a) a.setAttribute('target', '_self');
  }, true);
})()"#;

fn element_js(reference: u32, body: &str) -> String {
    format!(
        "(() => {{ const el = document.querySelector('[data-agentcore-ref=\"{reference}\"]'); \
         if (!el) return 'missing'; {body} }})()"
    )
}

impl Page {
    /// Start a browser for `access` with the screen of `device`.
    pub async fn launch(
        executable: &Path,
        access: Access,
        device: ReviewDevice,
        sandbox: bool,
    ) -> Result<Self, String> {
        let profile = tempfile::Builder::new()
            .prefix("agentcore-review-")
            .tempdir()
            .map_err(|e| format!("could not make the browser profile: {e}"))?;
        let mut cmd = Command::new(executable);
        cmd.args([
            "--headless=new",
            "--remote-debugging-port=0",
            "--no-first-run",
            "--no-default-browser-check",
            "--disable-extensions",
            "--disable-background-networking",
            "--disable-component-update",
            "--disable-default-apps",
            "--disable-sync",
            "--disable-dev-shm-usage",
            "--disable-site-isolation-trials",
            "--disable-features=IsolateOrigins,site-per-process,Translate,MediaRouter,OptimizationHints",
            "--mute-audio",
            "--hide-scrollbars",
            "--password-store=basic",
            "--use-mock-keychain",
        ])
        .arg(format!("--user-data-dir={}", profile.path().display()));
        if !sandbox || running_as_root() {
            // Chromium's own sandbox cannot run as root (e.g. in the
            // container image) or where user namespaces are not allowed.
            cmd.arg("--no-sandbox");
        }
        cmd.arg("about:blank")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        let mut child = cmd
            .spawn()
            .map_err(|e| format!("could not start Chromium ({}): {e}", executable.display()))?;
        let stderr = child.stderr.take().ok_or("no browser output")?;
        let mut lines = BufReader::new(stderr).lines();
        let ws = tokio::time::timeout(Duration::from_secs(30), async {
            while let Ok(Some(line)) = lines.next_line().await {
                if let Some(url) = line.split("DevTools listening on ").nth(1) {
                    return Some(url.trim().to_string());
                }
            }
            None
        })
        .await
        .ok()
        .flatten()
        .ok_or("Chromium did not start")?;
        // Keep reading so the browser never blocks on a full pipe.
        tokio::spawn(async move { while let Ok(Some(_)) = lines.next_line().await {} });

        let (socket, _) = tokio_tungstenite::connect_async(ws.as_str())
            .await
            .map_err(|e| format!("could not connect to Chromium: {e}"))?;
        let (mut sink, mut stream) = socket.split();
        let (out, mut outbox) = mpsc::unbounded_channel::<String>();
        tokio::spawn(async move {
            while let Some(msg) = outbox.recv().await {
                if sink.send(Message::text(msg)).await.is_err() {
                    break;
                }
            }
        });
        let cdp = Arc::new(Cdp {
            out,
            next: AtomicU64::new(1),
            pending: Mutex::default(),
        });
        let guard = Arc::new(Guard {
            access,
            main_frame: Mutex::default(),
            page_session: Mutex::default(),
            checked: tokio::sync::Mutex::default(),
            notes: Mutex::default(),
            left: std::sync::atomic::AtomicBool::new(false),
        });
        {
            let (cdp, guard) = (cdp.clone(), guard.clone());
            tokio::spawn(async move {
                while let Some(Ok(msg)) = stream.next().await {
                    let Ok(text) = msg.to_text() else { continue };
                    let Ok(value) = serde_json::from_str::<Value>(text) else {
                        continue;
                    };
                    if let Some(id) = value["id"].as_u64() {
                        let reply = cdp
                            .pending
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .remove(&id);
                        if let Some(reply) = reply {
                            let result = match value.get("error") {
                                Some(err) => {
                                    Err(err["message"].as_str().unwrap_or("error").to_string())
                                }
                                None => Ok(value["result"].clone()),
                            };
                            let _ = reply.send(result);
                        }
                    } else if value.get("method").is_some() {
                        tokio::spawn(handle_event(cdp.clone(), guard.clone(), value));
                    }
                }
                // Closed: fail whatever still waits.
                cdp.pending
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clear();
            });
        }

        let target = cdp
            .send(None, "Target.createTarget", json!({ "url": "about:blank" }))
            .await?;
        let target_id = target["targetId"].as_str().unwrap_or_default().to_string();
        let attached = cdp
            .send(
                None,
                "Target.attachToTarget",
                json!({ "targetId": target_id, "flatten": true }),
            )
            .await?;
        let session = attached["sessionId"]
            .as_str()
            .ok_or("could not attach to the page")?
            .to_string();
        *guard.main_frame.lock().unwrap_or_else(|e| e.into_inner()) = target_id;
        *guard.page_session.lock().unwrap_or_else(|e| e.into_inner()) = session.clone();
        let s = Some(session.as_str());
        cdp.send(s, "Page.enable", json!({})).await?;
        cdp.send(s, "Network.enable", json!({})).await?;
        cdp.send(
            s,
            "Network.setBypassServiceWorker",
            json!({ "bypass": true }),
        )
        .await?;
        cdp.send(
            s,
            "Fetch.enable",
            json!({ "patterns": [{ "urlPattern": "*" }] }),
        )
        .await?;
        cdp.send(
            s,
            "Target.setAutoAttach",
            json!({ "autoAttach": true, "waitForDebuggerOnStart": true, "flatten": true }),
        )
        .await?;
        cdp.send(
            s,
            "Page.addScriptToEvaluateOnNewDocument",
            json!({ "source": SAME_PAGE_JS }),
        )
        .await?;
        let page = Self {
            cdp,
            guard,
            session,
            child: tokio::sync::Mutex::new(child),
            _profile: profile,
        };
        page.set_device(device).await?;
        Ok(page)
    }

    async fn cmd(&self, method: &str, params: Value) -> Result<Value, String> {
        self.cdp.send(Some(&self.session), method, params).await
    }

    async fn eval(&self, expression: &str, wait: bool) -> Result<Value, String> {
        let reply = self
            .cmd(
                "Runtime.evaluate",
                json!({ "expression": expression, "returnByValue": true, "awaitPromise": wait }),
            )
            .await?;
        if let Some(ex) = reply.get("exceptionDetails") {
            let text = ex["exception"]["description"]
                .as_str()
                .or(ex["text"].as_str())
                .unwrap_or("script error");
            return Err(text.to_string());
        }
        Ok(reply["result"]["value"].clone())
    }

    /// Wait for the page to load and stop changing; back on the preview if
    /// the page tried to leave it.
    async fn settle(&self) {
        self.wait().await;
        if self.guard.left.swap(false, Ordering::SeqCst) {
            let _ = self.eval("history.back(); true", false).await;
            self.wait().await;
        }
    }

    async fn wait(&self) {
        tokio::time::sleep(Duration::from_millis(250)).await;
        let deadline = tokio::time::Instant::now() + SETTLE_TIMEOUT;
        while tokio::time::Instant::now() < deadline {
            if let Ok(state) = self.eval("document.readyState", false).await
                && state == "complete"
            {
                let _ = self.eval(SETTLE_JS, true).await;
                return;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }

    pub async fn set_device(&self, device: ReviewDevice) -> Result<(), String> {
        let (width, height, scale, mobile, ua) = match device {
            ReviewDevice::Phone => (390, 844, 2, true, PHONE_UA),
            ReviewDevice::Desktop => (1280, 800, 1, false, DESKTOP_UA),
        };
        self.cmd(
            "Emulation.setDeviceMetricsOverride",
            json!({ "width": width, "height": height, "deviceScaleFactor": scale, "mobile": mobile }),
        )
        .await?;
        self.cmd(
            "Emulation.setTouchEmulationEnabled",
            json!({ "enabled": mobile }),
        )
        .await?;
        self.cmd("Emulation.setUserAgentOverride", json!({ "userAgent": ua }))
            .await?;
        Ok(())
    }

    /// Open `url`, which must be on the preview (or an allowed host).
    pub async fn open(&self, url: &Url) -> Result<(), String> {
        if !self.guard.access.may_open(url) {
            return Err(format!(
                "{url} is not on the preview ({}); the review only opens the preview",
                self.guard.access.preview.origin().ascii_serialization()
            ));
        }
        let reply = self
            .cmd("Page.navigate", json!({ "url": url.as_str() }))
            .await?;
        if let Some(err) = reply["errorText"].as_str().filter(|e| !e.is_empty()) {
            return Err(format!("could not open {url}: {err}"));
        }
        self.settle().await;
        Ok(())
    }

    async fn point(&self, reference: u32) -> Result<(f64, f64), String> {
        let found = self
            .eval(
                &element_js(
                    reference,
                    "el.scrollIntoView({ block: 'center', inline: 'center' }); \
                     const r = el.getBoundingClientRect(); \
                     return JSON.stringify({ x: r.left + r.width / 2, y: r.top + r.height / 2 });",
                ),
                false,
            )
            .await?;
        let text = found.as_str().unwrap_or("missing");
        if text == "missing" {
            return Err(stale(reference));
        }
        let p: Value = serde_json::from_str(text).map_err(|e| e.to_string())?;
        Ok((
            p["x"].as_f64().unwrap_or(0.0),
            p["y"].as_f64().unwrap_or(0.0),
        ))
    }

    pub async fn click(&self, reference: u32) -> Result<(), String> {
        let (x, y) = self.point(reference).await?;
        for kind in ["mouseMoved", "mousePressed", "mouseReleased"] {
            self.cmd(
                "Input.dispatchMouseEvent",
                json!({ "type": kind, "x": x, "y": y, "button": "left", "clickCount": 1 }),
            )
            .await?;
        }
        self.settle().await;
        Ok(())
    }

    /// Replace the text in a field with `text`; with `submit`, press Enter.
    pub async fn type_text(&self, reference: u32, text: &str, submit: bool) -> Result<(), String> {
        let focused = self
            .eval(
                &element_js(
                    reference,
                    "el.scrollIntoView({ block: 'center' }); el.focus(); \
                     if (typeof el.select === 'function') el.select(); \
                     else if (el.isContentEditable) { const r = document.createRange(); \
                       r.selectNodeContents(el); const s = getSelection(); s.removeAllRanges(); s.addRange(r); } \
                     return 'ok';",
                ),
                false,
            )
            .await?;
        if focused != "ok" {
            return Err(stale(reference));
        }
        if text.is_empty() {
            self.press("Backspace").await?;
        } else {
            self.cmd("Input.insertText", json!({ "text": text }))
                .await?;
        }
        if submit {
            self.press("Enter").await?;
        }
        self.settle().await;
        Ok(())
    }

    pub async fn select(&self, reference: u32, option: &str) -> Result<(), String> {
        let want = serde_json::to_string(option).unwrap_or_default();
        let body = format!(
            "if (el.tagName !== 'SELECT') return 'not a select'; \
             const want = {want}; \
             const o = Array.from(el.options).find((o) => o.value === want || o.text.trim() === want); \
             if (!o) return 'no option'; el.value = o.value; \
             el.dispatchEvent(new Event('input', {{ bubbles: true }})); \
             el.dispatchEvent(new Event('change', {{ bubbles: true }})); return 'ok';"
        );
        match self
            .eval(&element_js(reference, &body), false)
            .await?
            .as_str()
        {
            Some("ok") => {
                self.settle().await;
                Ok(())
            }
            Some("missing") => Err(stale(reference)),
            Some("no option") => Err(format!("[{reference}] has no option {option:?}")),
            _ => Err(format!("[{reference}] is not a drop-down list")),
        }
    }

    pub async fn press(&self, key: &str) -> Result<(), String> {
        let (code, text, vk) = match key {
            "Enter" => ("Enter", "\r", 13),
            "Escape" => ("Escape", "", 27),
            "Tab" => ("Tab", "", 9),
            "Backspace" => ("Backspace", "", 8),
            "ArrowDown" => ("ArrowDown", "", 40),
            "ArrowUp" => ("ArrowUp", "", 38),
            "ArrowLeft" => ("ArrowLeft", "", 37),
            "ArrowRight" => ("ArrowRight", "", 39),
            "Space" => ("Space", " ", 32),
            other => return Err(format!("unknown key {other:?}")),
        };
        let key_name = if key == "Space" { " " } else { key };
        let mut down = json!({ "type": "keyDown", "key": key_name, "code": code,
                               "windowsVirtualKeyCode": vk, "nativeVirtualKeyCode": vk });
        if !text.is_empty() {
            down["text"] = json!(text);
        }
        self.cmd("Input.dispatchKeyEvent", down).await?;
        self.cmd(
            "Input.dispatchKeyEvent",
            json!({ "type": "keyUp", "key": key_name, "code": code,
                    "windowsVirtualKeyCode": vk, "nativeVirtualKeyCode": vk }),
        )
        .await?;
        self.settle().await;
        Ok(())
    }

    pub async fn scroll(&self, down: bool) -> Result<(), String> {
        let sign = if down { "" } else { "-" };
        self.eval(
            &format!("window.scrollBy(0, {sign}Math.round(window.innerHeight * 0.8)); true"),
            false,
        )
        .await?;
        self.settle().await;
        Ok(())
    }

    pub async fn back(&self) -> Result<(), String> {
        self.eval("history.back(); true", false).await?;
        self.settle().await;
        Ok(())
    }

    pub async fn snapshot(&self) -> Result<Snapshot, String> {
        let js = SNAPSHOT_JS.replace("MAX", &MAX_ELEMENTS.to_string());
        let raw = self.eval(&js, false).await?;
        let v: Value =
            serde_json::from_str(raw.as_str().unwrap_or("{}")).map_err(|e| e.to_string())?;
        Ok(Snapshot {
            url: v["url"].as_str().unwrap_or_default().to_string(),
            title: v["title"].as_str().unwrap_or_default().to_string(),
            text: v["text"].as_str().unwrap_or_default().to_string(),
            elements: v["elements"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|e| e.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default(),
        })
    }

    pub async fn current_url(&self) -> String {
        self.eval("location.href", false)
            .await
            .ok()
            .and_then(|v| v.as_str().map(String::from))
            .unwrap_or_default()
    }

    /// The visible screen as a JPEG.
    pub async fn screenshot(&self) -> Result<Vec<u8>, String> {
        let shot = self
            .cmd(
                "Page.captureScreenshot",
                json!({ "format": "jpeg", "quality": 70 }),
            )
            .await?;
        base64::engine::general_purpose::STANDARD
            .decode(shot["data"].as_str().unwrap_or_default())
            .map_err(|e| e.to_string())
    }

    /// What happened since the last call that the agent should know.
    pub fn take_notes(&self) -> Vec<String> {
        std::mem::take(&mut *self.guard.notes.lock().unwrap_or_else(|e| e.into_inner()))
    }

    pub async fn close(&self) {
        let _ = self.cdp.send(None, "Browser.close", json!({})).await;
        let mut child = self.child.lock().await;
        if tokio::time::timeout(Duration::from_secs(5), child.wait())
            .await
            .is_err()
        {
            let _ = child.kill().await;
        }
    }
}

fn stale(reference: u32) -> String {
    format!(
        "[{reference}] is no longer on the page: look at the page again (`browser_look`) and use the new numbers"
    )
}

#[cfg(unix)]
fn running_as_root() -> bool {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata("/proc/self").is_ok_and(|m| m.uid() == 0)
}

#[cfg(not(unix))]
fn running_as_root() -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn access() -> Access {
        Access {
            preview: Url::parse("https://demo-git-fix-acme.vercel.app").unwrap(),
            extra_hosts: vec!["accounts.acme.dev".into(), "*.clerk.dev".into()],
            bypass_secret: Some("s3cret".into()),
        }
    }

    #[test]
    fn only_the_preview_and_listed_hosts_are_opened() {
        let a = access();
        let open = |u: &str| a.may_open(&Url::parse(u).unwrap());
        assert!(open("https://demo-git-fix-acme.vercel.app/customers?x=1"));
        assert!(open("https://accounts.acme.dev/sign-in"));
        assert!(open("https://eu.clerk.dev/x"));
        assert!(
            !open("http://demo-git-fix-acme.vercel.app/"),
            "other scheme"
        );
        assert!(!open("https://evil.example.com/"));
        assert!(!open("https://clerk.dev.evil.com/"));
        assert!(!open("file:///etc/passwd"));
        assert!(!open("http://169.254.169.254/latest/meta-data"));
    }

    #[test]
    fn what_a_page_loads_from_elsewhere_is_checked() {
        let a = access();
        let route = |u: &str| a.route(&Url::parse(u).unwrap(), false);
        assert_eq!(
            route("https://demo-git-fix-acme.vercel.app/_next/a.js"),
            Route::Preview
        );
        assert_eq!(
            route("https://fonts.gstatic.com/x.woff2"),
            Route::Elsewhere {
                host: "fonts.gstatic.com".into(),
                port: 443
            }
        );
        assert_eq!(
            route("http://[::1]:8080/"),
            Route::Elsewhere {
                host: "::1".into(),
                port: 8080
            }
        );
        assert!(matches!(route("ftp://x/"), Route::Blocked(_)));
    }

    #[test]
    fn internal_addresses_are_recognised() {
        for ip in [
            "127.0.0.1",
            "10.1.2.3",
            "172.16.0.1",
            "192.168.1.1",
            "169.254.169.254",
            "100.64.0.1",
            "0.0.0.0",
            "::1",
            "fd00::1",
            "fe80::1",
            "::ffff:10.0.0.1",
        ] {
            assert!(is_internal(ip.parse().unwrap()), "{ip}");
        }
        for ip in ["8.8.8.8", "76.76.21.21", "2606:4700::1111"] {
            assert!(!is_internal(ip.parse().unwrap()), "{ip}");
        }
    }

    #[tokio::test]
    async fn private_hosts_are_refused() {
        assert!(public_host("127.0.0.1", 80).await.is_err());
        assert!(public_host("localhost", 80).await.is_err());
        assert!(public_host("10.0.0.8", 80).await.is_err());
    }

    #[test]
    fn snapshot_lists_controls_and_text() {
        let s = Snapshot {
            url: "https://p/".into(),
            title: "Home".into(),
            text: "Welcome".into(),
            elements: vec!["[1] button \"Save\"".into()],
        };
        let text = s.render(&["Blocked x".into()]);
        assert!(text.contains("Note: Blocked x"));
        assert!(text.contains("[1] button \"Save\""));
        assert!(text.ends_with("Welcome"));
    }

    /// Drives a real Chromium when one is installed (CI has one); skipped
    /// otherwise.
    #[tokio::test]
    async fn drives_a_preview_and_keeps_it_contained() {
        use axum::extract::State;
        use axum::http::HeaderMap;
        use axum::response::Html;
        use axum::routing::get;
        let Some(chromium) = find_chromium(None) else {
            eprintln!("no Chromium found: skipped");
            return;
        };
        // Something on another local port the page must not reach.
        let other = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let other_addr = other.local_addr().unwrap();
        let reached = Arc::new(AtomicU64::new(0));
        {
            let reached = reached.clone();
            let app = axum::Router::new().fallback(move || {
                reached.fetch_add(1, Ordering::SeqCst);
                async { "secret" }
            });
            tokio::spawn(async move { axum::serve(other, app).await.unwrap() });
        }
        let secrets: Arc<Mutex<Vec<String>>> = Arc::default();
        let page_html = format!(
            r#"<html><head><title>Customers</title></head><body>
            <h1>Customers</h1>
            <form onsubmit="event.preventDefault(); document.getElementById('out').textContent = 'Added ' + document.getElementById('name').value;">
              <label for="name">Name</label><input id="name">
              <select id="plan"><option value="free">Free</option><option value="pro">Pro</option></select>
              <button type="submit">Add customer</button>
            </form>
            <p id="out"></p>
            <a href="https://example.com/">Leave</a>
            <script>fetch('http://{other_addr}/x').catch(() => {{}});</script>
            </body></html>"#
        );
        let preview = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = preview.local_addr().unwrap();
        let app = axum::Router::new()
            .route(
                "/",
                get(
                    |State((html, seen)): State<(String, Arc<Mutex<Vec<String>>>)>,
                     headers: HeaderMap| async move {
                        if let Some(v) = headers.get("x-vercel-protection-bypass") {
                            seen.lock().unwrap().push(v.to_str().unwrap().to_string());
                        }
                        Html(html)
                    },
                ),
            )
            .with_state((page_html, secrets.clone()));
        tokio::spawn(async move { axum::serve(preview, app).await.unwrap() });

        let access = Access {
            preview: Url::parse(&format!("http://{addr}")).unwrap(),
            extra_hosts: vec![],
            bypass_secret: Some("bypass-me".into()),
        };
        let page = Page::launch(
            &chromium,
            access,
            ReviewDevice::Phone,
            sandbox_allowed(true),
        )
        .await
        .unwrap();
        page.open(&Url::parse(&format!("http://{addr}/")).unwrap())
            .await
            .unwrap();
        let snap = page.snapshot().await.unwrap();
        assert_eq!(snap.title, "Customers");
        let find = |label: &str| -> u32 {
            let line = snap
                .elements
                .iter()
                .find(|e| e.contains(label))
                .unwrap_or_else(|| panic!("{label} in {:?}", snap.elements));
            line[1..line.find(']').unwrap()].parse().unwrap()
        };
        page.type_text(find("\"Name\""), "Ana", false)
            .await
            .unwrap();
        page.select(find("select"), "Pro").await.unwrap();
        page.click(find("Add customer")).await.unwrap();
        let after = page.snapshot().await.unwrap();
        assert!(after.text.contains("Added Ana"), "{}", after.text);
        assert!(
            after
                .elements
                .iter()
                .any(|e| e.contains("selected=\"Pro\""))
        );
        let shot = page.screenshot().await.unwrap();
        assert_eq!(&shot[..2], &[0xff, 0xd8], "a JPEG");

        // Leaving the preview is refused, by the tool and in the page.
        assert!(
            page.open(&Url::parse("https://example.com/").unwrap())
                .await
                .is_err()
        );
        page.click(find("Leave")).await.unwrap();
        assert_eq!(page.current_url().await, format!("http://{addr}/"));
        let notes = page.take_notes();
        assert!(notes.iter().any(|n| n.contains("example.com")), "{notes:?}");
        assert!(
            notes.iter().any(|n| n.contains(&other_addr.to_string())),
            "{notes:?}"
        );
        assert_eq!(
            reached.load(Ordering::SeqCst),
            0,
            "the other local server was reached"
        );
        assert!(secrets.lock().unwrap().iter().all(|s| s == "bypass-me"));
        assert!(
            !secrets.lock().unwrap().is_empty(),
            "the preview got the bypass secret"
        );
        page.close().await;
    }
}
