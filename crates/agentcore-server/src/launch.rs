//! Telling people about what agents build.
//!
//! * A public **product page** (`/p/<product>`, opt-in per product): what it
//!   is, the current version with downloads and their SHA-256, the
//!   changelog, an Atom feed, and a sign-up form for news (with the consent
//!   people give, a developer/business choice, and an unsubscribe link).
//! * **Tracked links** (`?src=<channel>`): visits, downloads and sign-ups
//!   are counted per source and day. No cookies, no IP addresses, nothing
//!   per person.
//! * A **launch brief**: when a release reaches everyone, the departments
//!   that announce the product (marketing, content, developer relations)
//!   are told what shipped, for whom and with which links; their agents
//!   draft announcements into the outbox, where people approve them.
//! * `launch_*` tools for those agents: the facts, tracked links, results.

use std::fmt::Write as _;

use agentcore_core::releases::{Bump, clean_source};
use agentcore_core::{Audience, DepartmentId, OrgAgentId, Product, Release, ReleaseStatus};
use agentcore_runtime::ToolHandler;
use agentcore_store::{LaunchEvent, NewSubscriber, Segment, StoreError};
use async_trait::async_trait;
use axum::Form;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Redirect, Response};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::AppState;
use crate::auth::Caller;
use crate::error::ApiError;
use crate::org::{self, Recipient, Sender, def, text_arg};

type ApiResult<T> = Result<T, ApiError>;

/// What people agree to when they sign up.
pub fn consent_text(product: &Product) -> String {
    format!(
        "Send me news about {} (new versions and important changes). I can unsubscribe at any time.",
        product.name
    )
}

/// Base URL people reach this server at.
pub fn public_base(state: &AppState) -> String {
    state
        .config
        .server
        .public_url
        .as_deref()
        .map(|u| u.trim_end_matches('/').to_string())
        .unwrap_or_else(|| format!("http://{}", state.config.server.bind))
}

/// The product page, with `?src=` when a source is given.
pub fn page_url(state: &AppState, product: &str, source: Option<&str>) -> String {
    let base = format!("{}/p/{product}", public_base(state));
    match source {
        Some(s) => format!("{base}?src={}", clean_source(Some(s))),
        None => base,
    }
}

/// HTML-escape text (also for attributes).
pub fn esc(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

/// A plausible email address (the newsletter service confirms it).
pub fn valid_email(email: &str) -> bool {
    let Some((local, domain)) = email.split_once('@') else {
        return false;
    };
    (3..=254).contains(&email.len())
        && !local.is_empty()
        && local.len() <= 64
        && domain.contains('.')
        && !domain.starts_with('.')
        && !domain.ends_with('.')
        && !domain.contains('@')
        && email
            .chars()
            .all(|c| !c.is_whitespace() && !c.is_control() && !"<>,;\"()[]\\".contains(c))
}

/// A CSV cell; never read as a formula by spreadsheets.
pub fn csv_cell(value: &str) -> String {
    let value = if value.starts_with(['=', '+', '-', '@', '\t', '\r']) {
        format!("'{value}")
    } else {
        value.to_string()
    };
    format!("\"{}\"", value.replace('"', "\"\""))
}

/// Released versions of a product, newest first.
async fn released(state: &AppState, product: &Product) -> Result<Vec<Release>, StoreError> {
    let mut list = state
        .store
        .list_releases(Some(product.id), Some(ReleaseStatus::Released), None, 50)
        .await?;
    list.sort_by_key(|r| std::cmp::Reverse(r.released_at));
    Ok(list)
}

// ---- public pages -------------------------------------------------------------------

fn html(status: StatusCode, body: String) -> Response {
    let mut response = (status, body).into_response();
    let h = response.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/html; charset=utf-8"),
    );
    h.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(
            "default-src 'none'; style-src 'unsafe-inline'; form-action 'self'; \
             base-uri 'none'; frame-ancestors 'none'",
        ),
    );
    h.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    h.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("strict-origin-when-cross-origin"),
    );
    response
}

const STYLE: &str = r#"
:root { --bg:#fbfaf7; --fg:#1f1e1b; --muted:#6b6a65; --card:#fff; --line:#e5e3dc; --accent:#2a63c7; --ok:#1d7a3a; }
@media (prefers-color-scheme: dark) { :root { --bg:#151514; --fg:#ecebe6; --muted:#a4a39c; --card:#1e1e1c; --line:#34332f; --accent:#7aa7f0; --ok:#6fcf8f; } }
* { box-sizing: border-box; }
body { margin:0; background:var(--bg); color:var(--fg); font: 16px/1.55 system-ui, -apple-system, "Segoe UI", sans-serif; }
main { max-width: 760px; margin: 0 auto; padding: 32px 16px 48px; display: grid; gap: 20px; }
h1 { font-size: 34px; margin: 0; line-height: 1.15; } h2 { font-size: 19px; margin: 0 0 8px; }
.tagline { font-size: 19px; color: var(--muted); margin: 6px 0 0; }
.card { background: var(--card); border: 1px solid var(--line); border-radius: 12px; padding: 18px; }
.muted { color: var(--muted); font-size: 14px; } .pill { display:inline-block; border:1px solid var(--line); border-radius:999px; padding:1px 10px; font-size:13px; }
.btn { display:inline-block; background: var(--accent); color: #fff; padding: 9px 16px; border-radius: 8px; text-decoration: none; font-weight: 600; border: 0; font-size: 15px; cursor: pointer; }
.downloads { display: grid; gap: 10px; } .dl { display:flex; flex-wrap:wrap; gap:8px 14px; align-items:center; }
code, .mono { font-family: ui-monospace, Menlo, Consolas, monospace; font-size: 12.5px; overflow-wrap: anywhere; }
.notes { white-space: pre-wrap; margin: 6px 0 0; }
.release + .release { border-top: 1px solid var(--line); margin-top: 14px; padding-top: 14px; }
form { display: grid; gap: 10px; } input[type=email], input[type=text] { width: 100%; padding: 9px 10px; border-radius: 8px; border: 1px solid var(--line); background: var(--bg); color: var(--fg); font-size: 15px; }
.row { display:flex; flex-wrap:wrap; gap: 14px; } .hp { position:absolute; left:-10000px; width:1px; height:1px; overflow:hidden; }
a { color: var(--accent); } footer { font-size: 13px; color: var(--muted); }
"#;

fn layout(title: &str, body: &str) -> String {
    format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
         <title>{}</title><style>{STYLE}</style></head><body><main>{body}</main></body></html>",
        esc(title)
    )
}

fn not_found() -> Response {
    html(
        StatusCode::NOT_FOUND,
        layout(
            "Not found",
            "<h1>Not found</h1><p class=\"muted\">There is no such product page.</p>",
        ),
    )
}

/// The product, if it has a public page.
async fn public_product(state: &AppState, name: &str) -> Option<Product> {
    match state.store.product_named(name).await {
        Ok(p) if p.public && p.enabled => Some(p),
        _ => None,
    }
}

async fn count(state: &AppState, product: &Product, source: &str, kind: LaunchEvent) {
    if let Err(err) = state
        .store
        .record_launch_event(product.id, source, kind)
        .await
    {
        tracing::debug!(error = %err, "could not count a launch event");
    }
}

#[derive(Debug, Deserialize)]
pub struct SourceQuery {
    #[serde(default)]
    pub src: Option<String>,
}

fn render_page(
    product: &Product,
    released: &[Release],
    source: &str,
    notice: Option<&str>,
) -> String {
    let mut b = String::new();
    let current = released.first();
    let _ = write!(
        b,
        "<header><h1>{}</h1>{}<p class=\"muted\">{}{}</p></header>",
        esc(&product.name),
        if product.tagline.is_empty() {
            String::new()
        } else {
            format!("<p class=\"tagline\">{}</p>", esc(&product.tagline))
        },
        current.map_or_else(
            || "Not released yet".to_string(),
            |r| format!("<span class=\"pill\">Version {}</span>", esc(&r.version)),
        ),
        product.website.as_deref().map_or_else(String::new, |w| {
            format!(" · <a href=\"{}\" rel=\"noopener\">Website</a>", esc(w))
        }),
    );
    if let Some(n) = notice {
        let _ = write!(b, "<div class=\"card\"><strong>{}</strong></div>", esc(n));
    }
    if !product.description.is_empty() {
        let _ = write!(b, "<p>{}</p>", esc(&product.description));
    }
    if let Some(r) = current {
        let _ = write!(
            b,
            "<section class=\"card\"><h2>Download</h2><div class=\"downloads\">"
        );
        for a in &r.artifacts {
            let _ = write!(
                b,
                "<div><div class=\"dl\"><a class=\"btn\" href=\"/p/{}/download/{}?src={}\">{}</a>\
                 <span class=\"muted\">{}{}</span></div>\
                 <div class=\"muted\">SHA-256 <code>{}</code></div></div>",
                esc(&product.name),
                esc(&url_segment(&a.name)),
                esc(source),
                esc(&a.platform),
                esc(&a.name),
                a.size
                    .map_or_else(String::new, |s| format!(" · {}", human_size(s))),
                esc(&a.sha256),
            );
        }
        let _ = write!(
            b,
            "</div><p class=\"muted\">Check the SHA-256 of what you downloaded \
             (<code>sha256sum &lt;file&gt;</code>). Releases are signed; update clients verify \
             them with the keys at <a href=\"/dist/keys\">/dist/keys</a>.</p></section>"
        );
    }
    let _ = write!(
        b,
        "<section class=\"card\"><h2>Get news about {name}</h2>\
         <form method=\"post\" action=\"/p/{name}/subscribe\">\
         <input type=\"hidden\" name=\"src\" value=\"{src}\">\
         <label class=\"hp\" aria-hidden=\"true\">Leave empty <input type=\"text\" name=\"website\" tabindex=\"-1\" autocomplete=\"off\"></label>\
         <label>Email <input type=\"email\" name=\"email\" required maxlength=\"254\" autocomplete=\"email\"></label>\
         <div class=\"row\"><label><input type=\"radio\" name=\"segment\" value=\"developer\"{dev}> I'm a developer</label>\
         <label><input type=\"radio\" name=\"segment\" value=\"business\"{biz}> I'm here for a business</label></div>\
         <label>Company (optional) <input type=\"text\" name=\"company\" maxlength=\"120\" autocomplete=\"organization\"></label>\
         <label><input type=\"checkbox\" name=\"consent\" value=\"yes\" required> {consent}</label>\
         <div><button class=\"btn\" type=\"submit\">Subscribe</button></div></form></section>",
        name = esc(&product.name),
        src = esc(source),
        dev = if product.audience == Audience::Business {
            ""
        } else {
            " checked"
        },
        biz = if product.audience == Audience::Business {
            " checked"
        } else {
            ""
        },
        consent = esc(&consent_text(product)),
    );
    if !released.is_empty() {
        let _ = write!(
            b,
            "<section class=\"card\"><h2>Changelog</h2><p class=\"muted\"><a href=\"/p/{}/feed.xml\">Atom feed</a></p>",
            esc(&product.name)
        );
        for r in released.iter().take(20) {
            let _ = write!(
                b,
                "<article class=\"release\"><strong>{}</strong> <span class=\"muted\">{}</span>\
                 <p class=\"notes\">{}</p></article>",
                esc(&r.version),
                r.released_at
                    .map(|d| d.format("%Y-%m-%d").to_string())
                    .unwrap_or_default(),
                esc(&r.notes)
            );
        }
        b.push_str("</section>");
    }
    b.push_str(
        "<footer>Built and released with agentcore. AI agents wrote parts of this software and \
         its release notes; people reviewed and approved every release.</footer>",
    );
    let title = if product.tagline.is_empty() {
        product.name.clone()
    } else {
        format!("{} – {}", product.name, product.tagline)
    };
    layout(&title, &b)
}

/// `18.7 MB`, `412 KB`, `96 bytes`.
pub fn human_size(bytes: u64) -> String {
    match bytes {
        0..1_000 => format!("{bytes} bytes"),
        1_000..1_000_000 => format!("{:.0} KB", bytes as f64 / 1e3),
        1_000_000..1_000_000_000 => format!("{:.1} MB", bytes as f64 / 1e6),
        _ => format!("{:.2} GB", bytes as f64 / 1e9),
    }
}

/// Artifact names are file names (no slashes); keep them safe in a path.
fn url_segment(name: &str) -> String {
    name.bytes()
        .map(|b| match b {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            other => format!("%{other:02X}"),
        })
        .collect()
}

pub async fn page(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Query(q): Query<SourceQuery>,
) -> Response {
    let Some(product) = public_product(&state, &name).await else {
        return not_found();
    };
    let source = clean_source(q.src.as_deref());
    count(&state, &product, &source, LaunchEvent::Visit).await;
    let released = released(&state, &product).await.unwrap_or_default();
    html(
        StatusCode::OK,
        render_page(&product, &released, &source, None),
    )
}

pub async fn download(
    State(state): State<AppState>,
    Path((name, artifact)): Path<(String, String)>,
    Query(q): Query<SourceQuery>,
) -> Response {
    let Some(product) = public_product(&state, &name).await else {
        return not_found();
    };
    let Ok(Some(current)) = state.store.baseline_release(product.id).await else {
        return not_found();
    };
    let Some(a) = current.artifacts.iter().find(|a| a.name == artifact) else {
        return not_found();
    };
    count(
        &state,
        &product,
        &clean_source(q.src.as_deref()),
        LaunchEvent::Download,
    )
    .await;
    Redirect::to(&a.url).into_response()
}

pub fn atom(base: &str, product: &Product, released: &[Release]) -> String {
    let page = format!("{base}/p/{}", product.name);
    let updated = released
        .first()
        .and_then(|r| r.released_at)
        .unwrap_or(product.updated_at)
        .to_rfc3339();
    let mut x = format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<feed xmlns=\"http://www.w3.org/2005/Atom\">\
         <title>{} releases</title><id>{}</id><updated>{updated}</updated>\
         <link href=\"{}\"/><link rel=\"self\" href=\"{}/feed.xml\"/>",
        esc(&product.name),
        esc(&page),
        esc(&page),
        esc(&page),
    );
    for r in released.iter().take(50) {
        let _ = write!(
            x,
            "<entry><title>{} {}</title><id>urn:uuid:{}</id><updated>{}</updated>\
             <link href=\"{}?src=feed\"/><content type=\"text\">{}</content></entry>",
            esc(&product.name),
            esc(&r.version),
            r.id,
            r.released_at.unwrap_or(r.updated_at).to_rfc3339(),
            esc(&page),
            esc(&r.notes)
        );
    }
    x.push_str("</feed>");
    x
}

pub async fn feed(State(state): State<AppState>, Path(name): Path<String>) -> Response {
    let Some(product) = public_product(&state, &name).await else {
        return not_found();
    };
    let released = released(&state, &product).await.unwrap_or_default();
    (
        [(header::CONTENT_TYPE, "application/atom+xml; charset=utf-8")],
        atom(&public_base(&state), &product, &released),
    )
        .into_response()
}

#[derive(Debug, Deserialize)]
pub struct SubscribeForm {
    #[serde(default)]
    pub email: String,
    #[serde(default)]
    pub segment: Option<String>,
    #[serde(default)]
    pub company: Option<String>,
    #[serde(default)]
    pub consent: Option<String>,
    #[serde(default)]
    pub src: Option<String>,
    /// Honeypot: people never fill it in.
    #[serde(default)]
    pub website: Option<String>,
}

fn random_token() -> String {
    let mut bytes = [0u8; 24];
    let _ = ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut bytes);
    hex::encode(bytes)
}

pub async fn subscribe(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Form(form): Form<SubscribeForm>,
) -> Response {
    let Some(product) = public_product(&state, &name).await else {
        return not_found();
    };
    let source = clean_source(form.src.as_deref());
    let released = released(&state, &product).await.unwrap_or_default();
    let again = |msg: &str| {
        html(
            StatusCode::BAD_REQUEST,
            render_page(&product, &released, &source, Some(msg)),
        )
    };
    let thanks = || {
        html(
            StatusCode::OK,
            layout(
                &product.name,
                &format!(
                    "<h1>Thank you</h1><p>You will hear from us about {} when there is news. \
                     Every message has a link to unsubscribe.</p><p><a href=\"/p/{}\">Back</a></p>",
                    esc(&product.name),
                    esc(&product.name)
                ),
            ),
        )
    };
    // Bots fill in every field; say thanks and keep nothing.
    if form
        .website
        .as_deref()
        .is_some_and(|w| !w.trim().is_empty())
    {
        return thanks();
    }
    let email = form.email.trim().to_string();
    if !valid_email(&email) {
        return again("Please enter a valid email address.");
    }
    if form.consent.as_deref() != Some("yes") {
        return again("Please tick the box to agree to receive news.");
    }
    let segment = match form.segment.as_deref() {
        Some("business") => Segment::Business,
        _ => Segment::Developer,
    };
    let company = form
        .company
        .as_deref()
        .map(str::trim)
        .filter(|c| !c.is_empty())
        .map(|c| c.chars().take(120).collect::<String>());
    let consent = consent_text(&product);
    let token = random_token();
    let added = state
        .store
        .add_subscriber(NewSubscriber {
            product: product.id,
            email: &email,
            segment,
            company: company.as_deref(),
            source: &source,
            consent: &consent,
            unsubscribe_token: &token,
        })
        .await;
    match added {
        Ok(true) => {
            count(&state, &product, &source, LaunchEvent::Signup).await;
            let token = state
                .store
                .unsubscribe_token(product.id, &email)
                .await
                .ok()
                .flatten()
                .unwrap_or(token);
            hand_over(
                &state,
                &product,
                "subscribed",
                json!({
                    "email": email, "segment": segment, "company": company,
                    "source": source, "consent": consent,
                    "unsubscribe_url": format!(
                        "{}/p/{}/unsubscribe?token={token}", public_base(&state), product.name
                    ),
                }),
            )
            .await;
            org::notify(&state.store, json!({ "kind": "launch" })).await;
            thanks()
        }
        // Already subscribed: the same answer (nobody learns who is on the list).
        Ok(false) => thanks(),
        Err(err) => {
            tracing::warn!(error = %err, "could not add a subscriber");
            again("Something went wrong; please try again later.")
        }
    }
}

/// Pass a sign-up on to the product's webhook (a newsletter service through
/// Zapier, Make or n8n), signed like every other event.
async fn hand_over(state: &AppState, product: &Product, event: &'static str, mut body: Value) {
    let Some(url) = product.webhook_url.clone() else {
        return;
    };
    body["event"] = json!(event);
    body["product"] = json!(product.name);
    body["at"] = json!(chrono::Utc::now());
    let secret = state.store.webhook_secret(product.id).await.ok().flatten();
    tokio::spawn(async move {
        if let Err(err) = crate::releases::post_webhook(&url, secret.as_deref(), event, &body).await
        {
            tracing::warn!(error = %err, "sign-up webhook failed");
        }
    });
}

#[derive(Debug, Deserialize)]
pub struct TokenInput {
    #[serde(default)]
    pub token: String,
}

/// A page with a button: link scanners in mail systems open links, so the
/// unsubscribe happens on the button press.
pub async fn unsubscribe_page(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Query(q): Query<TokenInput>,
) -> Response {
    let Some(product) = public_product(&state, &name).await else {
        return not_found();
    };
    html(
        StatusCode::OK,
        layout(
            &product.name,
            &format!(
                "<h1>Unsubscribe</h1><p>Stop news about {name}?</p>\
                 <form method=\"post\" action=\"/p/{name}/unsubscribe\">\
                 <input type=\"hidden\" name=\"token\" value=\"{token}\">\
                 <div><button class=\"btn\" type=\"submit\">Unsubscribe</button></div></form>",
                name = esc(&product.name),
                token = esc(&q.token),
            ),
        ),
    )
}

pub async fn unsubscribe(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Form(input): Form<TokenInput>,
) -> Response {
    let Some(product) = public_product(&state, &name).await else {
        return not_found();
    };
    if let Ok(Some(email)) = state
        .store
        .unsubscribe(product.id, input.token.trim())
        .await
    {
        hand_over(&state, &product, "unsubscribed", json!({ "email": email })).await;
        org::notify(&state.store, json!({ "kind": "launch" })).await;
    }
    html(
        StatusCode::OK,
        layout(
            &product.name,
            &format!(
                "<h1>Unsubscribed</h1><p>You will not get news about {} any more.</p>",
                esc(&product.name)
            ),
        ),
    )
}

// ---- the launch brief ---------------------------------------------------------------

/// Where announcements usually work, by audience. People post in
/// communities themselves; agents draft.
fn channel_advice(audience: Audience) -> &'static str {
    match audience {
        Audience::Developers => {
            "Developers: lead with what they can do now, show a short code or command example, \
             link the changelog and docs, be precise and modest (no superlatives). Good places: \
             the newsletter, the changelog/blog post, the project's GitHub release and \
             discussions, developer communities (Hacker News, Reddit, Discord, dev.to) and \
             social posts. Communities are posted to by a person: prepare the text for them."
        }
        Audience::Business => {
            "Businesses: lead with the problem it solves and the outcome (time, money, risk), \
             name who it is for, add a clear next step (try it, book a call). Good places: the \
             newsletter, LinkedIn, a customer email to existing customers, a short case or \
             use-case post. Never invent customers, numbers or quotes."
        }
        Audience::Both => {
            "Both developers and businesses: write separate drafts. For developers: what they \
             can do now, a short example, links to the changelog and docs, precise and modest; \
             developer communities are posted to by a person, so prepare the text for them. For \
             businesses: the problem and the outcome, who it is for, a clear next step, on the \
             newsletter and LinkedIn. Never invent customers, numbers or quotes."
        }
    }
}

/// The brief for the departments that announce a product.
pub fn brief_text(
    product: &Product,
    release: &Release,
    bump: Bump,
    page: &str,
    link: impl Fn(&str) -> String,
) -> String {
    let what = match bump {
        Bump::Initial => "its first release",
        Bump::Major => "a major version (breaking changes are possible: say what users must do)",
        Bump::Minor => "a minor version with new features",
        Bump::Patch => "a patch (fixes)",
        Bump::Prerelease => "a pre-release",
    };
    let mut t = format!(
        "Launch brief: {} {} is released to everyone. It is {what}.\n\n",
        product.name, release.version
    );
    if !product.tagline.is_empty() {
        let _ = writeln!(t, "Tagline: {}", product.tagline);
    }
    if !product.pitch.is_empty() {
        let _ = writeln!(t, "Positioning: {}", product.pitch);
    }
    let _ = writeln!(t, "Audience: {}", product.audience.as_str());
    let _ = writeln!(t, "\nRelease notes:\n{}\n", release.notes.trim());
    if product.public {
        let _ = writeln!(
            t,
            "Product page: {page}\nUse one tracked link per channel so we learn what works \
             (launch_link makes them), e.g. {} or {}.",
            link("newsletter"),
            link("linkedin")
        );
    }
    let _ = write!(
        t,
        "\n{}\n\nDraft the announcements with outbox_draft (a person approves each one). Only \
         claim what the notes say. Check later with launch_stats which channels brought \
         visits, downloads and sign-ups, and do more of what works.",
        channel_advice(product.audience)
    );
    t
}

/// Brief the announcing departments when a release reaches everyone.
pub async fn brief(state: &AppState, product: &Product, release: &Release) {
    if product.announce_departments.is_empty() {
        return;
    }
    let previous = released(state, product)
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(|r| r.id != release.id)
        .filter_map(|r| r.parsed_version())
        .filter(|v| release.parsed_version().is_some_and(|this| *v < this))
        .max();
    let Some(this) = release.parsed_version() else {
        return;
    };
    let bump = Bump::between(previous.as_ref(), &this);
    if matches!(bump, Bump::Patch | Bump::Prerelease) && !product.announce_patches {
        return;
    }
    let page = page_url(state, &product.name, None);
    let text = brief_text(product, release, bump, &page, |s| {
        page_url(state, &product.name, Some(s))
    });
    for department in &product.announce_departments {
        if let Err(err) = org::send(
            &state.store,
            &Sender::Human("releases".into()),
            Recipient::Department(*department),
            &text,
        )
        .await
        {
            tracing::warn!(error = %err, "could not send the launch brief");
        }
    }
}

// ---- agent tools --------------------------------------------------------------------

/// `launch_*` tools of a worker whose department announces a product.
pub struct LaunchTools {
    state: AppState,
    department: DepartmentId,
}

impl LaunchTools {
    pub fn new(state: AppState, _agent: OrgAgentId, department: DepartmentId) -> Self {
        Self { state, department }
    }
}

#[async_trait]
impl ToolHandler for LaunchTools {
    fn definitions(&self) -> Vec<Value> {
        vec![
            def(
                "launch_products",
                "Products your department announces: tagline, positioning, audience, the \
                 current version and its notes, the public page, and how many people subscribed.",
                json!({}),
                &[],
            ),
            def(
                "launch_link",
                "A tracked link to a product's page for one channel (`source`, e.g. newsletter, \
                 linkedin, hn, devto, partner-acme). Use a different source per channel and \
                 post: visits, downloads and sign-ups are counted per source.",
                json!({ "product": { "type": "string" }, "source": { "type": "string" } }),
                &["product", "source"],
            ),
            def(
                "launch_stats",
                "Visits, downloads and sign-ups of a product's page per source over the last \
                 `days` (default 30), and subscribers by segment. Use it to learn which channels \
                 and messages work.",
                json!({ "product": { "type": "string" }, "days": { "type": "integer" } }),
                &["product"],
            ),
        ]
    }

    async fn call(&self, tool: &str, arguments: &Value) -> Result<Value, String> {
        let store = &self.state.store;
        let products = store
            .products_announced_by(self.department)
            .await
            .map_err(|e| e.to_string())?;
        let named = |args: &Value| -> Result<Product, String> {
            let name = text_arg(args, "product")?;
            products
                .iter()
                .find(|p| p.name == name.trim())
                .cloned()
                .ok_or_else(|| {
                    format!("your department announces no product `{name}` (see launch_products)")
                })
        };
        match tool {
            "launch_products" => {
                let mut list = Vec::new();
                for p in &products {
                    let current = released(&self.state, p)
                        .await
                        .map_err(|e| e.to_string())?
                        .into_iter()
                        .next();
                    let stats = store
                        .launch_stats(p.id, 30)
                        .await
                        .map_err(|e| e.to_string())?;
                    list.push(json!({
                        "name": p.name, "tagline": p.tagline, "description": p.description,
                        "positioning": p.pitch, "audience": p.audience, "website": p.website,
                        "page": p.public.then(|| page_url(&self.state, &p.name, None)),
                        "current_version": current.as_ref().map(|r| &r.version),
                        "notes": current.as_ref().map(|r| &r.notes),
                        "released_at": current.as_ref().and_then(|r| r.released_at),
                        "subscribers": stats.subscribers,
                    }));
                }
                Ok(json!({ "products": list }))
            }
            "launch_link" => {
                let p = named(arguments)?;
                if !p.public {
                    return Err(format!(
                        "`{}` has no public page yet; an admin can turn it on",
                        p.name
                    ));
                }
                let raw = text_arg(arguments, "source")?;
                let source = clean_source(Some(&raw));
                if source == "other" || source == "direct" {
                    return Err("`source` is a short name: a-z, 0-9, -, _ and . (up to 40)".into());
                }
                Ok(json!({
                    "url": page_url(&self.state, &p.name, Some(&source)), "source": source,
                }))
            }
            "launch_stats" => {
                let p = named(arguments)?;
                let days = arguments["days"].as_u64().unwrap_or(30).clamp(1, 365) as u32;
                let stats = store
                    .launch_stats(p.id, days)
                    .await
                    .map_err(|e| e.to_string())?;
                Ok(json!(stats))
            }
            other => Err(format!("unknown tool `{other}`")),
        }
    }
}

// ---- metrics ------------------------------------------------------------------------

/// Launch results of every public product over `days`, for the dashboard
/// and the retrospective.
pub async fn overview(store: &agentcore_store::Store, days: u32) -> Result<Vec<Value>, StoreError> {
    let since = chrono::Utc::now() - chrono::Duration::days(i64::from(days));
    let mut out = Vec::new();
    for p in store.list_products().await? {
        if !p.public || !p.enabled {
            continue;
        }
        let stats = store.launch_stats(p.id, days).await?;
        let released_recently = store
            .list_releases(Some(p.id), Some(ReleaseStatus::Released), None, 100)
            .await?
            .iter()
            .filter(|r| r.released_at.is_some_and(|at| at >= since))
            .count();
        out.push(json!({
            "product": p.name,
            "released": released_recently,
            "visits": stats.visits,
            "downloads": stats.downloads,
            "signups": stats.signups,
            "subscribers": stats.subscribers,
            "sources": stats.sources,
            "announced_by": p.announce_departments,
        }));
    }
    Ok(out)
}

/// What looks wrong in the launch results.
pub fn signals(overview: &[Value]) -> Vec<crate::insights::Signal> {
    let mut out = Vec::new();
    for p in overview {
        let name = p["product"].as_str().unwrap_or_default();
        if p["released"].as_u64().unwrap_or(0) > 0 && p["visits"].as_i64().unwrap_or(0) == 0 {
            out.push(crate::insights::Signal {
                severity: "medium",
                kind: "launch_unseen",
                title: format!("Nobody visited {name}'s page after its release"),
                detail: "A new version shipped, but no one came to look at it.".into(),
                suggestion: if p["announced_by"].as_array().is_some_and(|a| a.is_empty()) {
                    "Let a marketing or developer relations department announce the product's \
                     releases, and share its page."
                        .into()
                } else {
                    "Check that the announcements were approved in the outbox and use tracked \
                     links."
                        .into()
                },
                department_id: None,
                agent_id: None,
                goal_id: None,
            });
        }
    }
    out
}

// ---- API ----------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct DaysQuery {
    #[serde(default)]
    pub days: Option<u32>,
}

pub async fn stats(
    State(state): State<AppState>,
    _caller: Caller,
    Path(id): Path<uuid::Uuid>,
    Query(q): Query<DaysQuery>,
) -> ApiResult<axum::Json<Value>> {
    let product = state.store.get_product(id).await?;
    let stats = state.store.launch_stats(id, q.days.unwrap_or(30)).await?;
    Ok(axum::Json(json!({
        "page": product.public.then(|| page_url(&state, &product.name, None)),
        "stats": stats,
    })))
}

/// Subscribers as CSV for a newsletter service (admin; logged).
pub async fn subscribers_csv(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<uuid::Uuid>,
) -> ApiResult<Response> {
    caller.require_admin()?;
    let product = state.store.get_product(id).await?;
    let rows = state
        .store
        .export_subscribers(&product, &caller.name)
        .await?;
    let mut csv = String::from("email,segment,company,source,subscribed_at,consent\n");
    for r in rows {
        let _ = writeln!(
            csv,
            "{},{},{},{},{},{}",
            csv_cell(&r.email),
            csv_cell(&r.segment),
            csv_cell(r.company.as_deref().unwrap_or_default()),
            csv_cell(&r.source),
            csv_cell(&r.subscribed_at.to_rfc3339()),
            csv_cell(&r.consent)
        );
    }
    Ok((
        [
            (header::CONTENT_TYPE, "text/csv; charset=utf-8".to_string()),
            (
                header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"{}-subscribers.csv\"", product.name),
            ),
        ],
        csv,
    )
        .into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escaping_and_cells() {
        assert_eq!(
            esc("<a href=\"x\">'&'</a>"),
            "&lt;a href=&quot;x&quot;&gt;&#39;&amp;&#39;&lt;/a&gt;"
        );
        assert_eq!(csv_cell("=HYPERLINK(1)"), "\"'=HYPERLINK(1)\"");
        assert_eq!(csv_cell("a \"b\""), "\"a \"\"b\"\"\"");
        assert_eq!(url_segment("app 1.0/x.tar.gz"), "app%201.0%2Fx.tar.gz");
        assert_eq!(human_size(96), "96 bytes");
        assert_eq!(human_size(412_000), "412 KB");
        assert_eq!(human_size(18_734_112), "18.7 MB");
    }

    #[test]
    fn emails() {
        for ok in ["ana@example.com", "a.b+tag@sub.example.io"] {
            assert!(valid_email(ok), "{ok}");
        }
        for bad in [
            "",
            "ana",
            "ana@",
            "@x.com",
            "ana@x",
            "a b@x.com",
            "a@x.com>",
            "a@@x.com",
            "a@.x.com",
        ] {
            assert!(!valid_email(bad), "{bad}");
        }
    }
}
