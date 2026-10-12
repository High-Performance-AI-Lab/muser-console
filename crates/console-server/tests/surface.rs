//! Console-terminated surface: dashboard asset, healthz, login-over-HTTP,
//! and the closed 404 fallback (no static-file traversal surface exists).

mod common;

use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE};

async fn console_only() -> std::net::SocketAddr {
    // No proxied route is exercised here; the instance is never contacted.
    let upstream = "127.0.0.1:1".parse().expect("addr");
    common::spawn_console(upstream).await
}

#[tokio::test]
async fn dashboard_served_byte_exact_without_auth() {
    let console = console_only().await;
    let client = common::client();
    let expected =
        std::fs::read(common::repo_root().join("ui/muser-dashboard.html")).expect("ui asset");

    for path in ["/", "/dashboard"] {
        let (parts, body) = common::request(&client, "GET", console, path, &[], b"").await;
        assert_eq!(parts.status, 200, "GET {path}");
        assert_eq!(
            parts.headers.get(CONTENT_TYPE).expect("content-type"),
            "text/html; charset=utf-8"
        );
        assert_eq!(
            parts.headers.get(CACHE_CONTROL).expect("cache-control"),
            "no-store"
        );
        assert!(
            body.as_ref() == expected,
            "GET {path} must serve the exact dashboard bytes"
        );
    }
}

#[test]
fn dashboard_keeps_demo_states_honest_and_comprehensible() {
    let dashboard = std::fs::read_to_string(common::repo_root().join("ui/muser-dashboard.html"))
        .expect("ui asset");

    // An idle snapshot retains both the last structural bytes and their
    // denominator. Keeping only the numerator made both layer bars clamp to
    // 100% as soon as the request-scoped session disappeared.
    assert!(dashboard.contains("total: reportedNothing(k.total_bytes)"));
    assert!(dashboard.contains("window.__kvClassLast && window.__kvClassLast.total"));
    assert!(dashboard.contains("last session · idle"));

    // Absence and zero are different readings. `||0` collapsed them, and the
    // panels printed "0 tok · 0 B" for fields nothing had reported.
    assert!(dashboard.contains("function reportedNothing(v)"));
    assert!(dashboard.contains("function fmtOrDash(v, fmtFn)"));
    assert!(!dashboard.contains("\"0 tok · 0 B\""));
    // An unreported uptime is a dash, not a process that just started.
    assert!(dashboard.contains("$(\"#uptime\").textContent = fmtOrDash(snap.uptime_s, fmtClock)"));

    // Loopback bearer mode intentionally polls because EventSource cannot
    // carry its in-memory Authorization header. That is a healthy transport,
    // not an SSE recovery state.
    assert!(dashboard.contains("startPollFallback(false)"));
    assert!(dashboard.contains("pollIsRecovery?\"reconnecting\":\"ok\""));

    // The native producer's sentinel remains untouched in telemetry, while
    // the visual surface explains its meaning and does not invent an identity.
    assert!(dashboard.contains("producerName === \"unsolicited-producer\""));
    assert!(dashboard.contains("external producer"));
    assert!(dashboard.contains("remote prefill"));
    assert!(dashboard.contains("node service offline; external producer active"));
    assert!(dashboard.contains("Topology — \" + name"));

    // Nothing reported a producer means no producer box: the canvas used to
    // draw a machine called "gx10-prefill" that nobody had ever mentioned.
    assert!(!dashboard.contains("gx10-prefill"));
    assert!(dashboard.contains("const hasReceipt = !!producerName;"));

    // The wordmark carries what the engine reported or says it has nothing.
    assert!(dashboard.contains("esc(c.model) || \"model unavailable\""));
    assert!(!dashboard.contains("c.model||\"Muse Glimmer-30B\""));

    // Absent speculative decoding is unavailable, not a row of synthetic
    // zero counters.
    assert!(dashboard.contains("<span class=\"tv\">not configured</span>"));
    assert!(!dashboard.contains("Sealed speed claims"));
}

#[test]
fn dashboard_shows_truthful_native_startup_milestones() {
    let dashboard = std::fs::read_to_string(common::repo_root().join("ui/muser-dashboard.html"))
        .expect("ui asset");

    for phase in [
        "Engine setup",
        "Load weights",
        "Init 8K chunks",
        "Allocate 128K KV",
        "Warm first request",
        "Ready",
    ] {
        assert!(dashboard.contains(phase), "missing startup phase: {phase}");
    }
    assert!(dashboard.contains("raw.data.native_startup"));
    assert!(dashboard.contains("role=\"progressbar\""));
    assert!(dashboard.contains("aria-valuemax=\"${value.total}\""));
    assert!(dashboard.contains("Startup milestones, not a time estimate"));
    assert!(dashboard.contains("nativeElapsed.textContent=nativeStartupElapsed"));
}

#[test]
fn add_node_tracks_same_process_activation_when_the_engine_offers_it() {
    let dashboard = std::fs::read_to_string(common::repo_root().join("ui/muser-dashboard.html"))
        .expect("ui asset");

    assert!(dashboard.contains("id=\"wzTarget\""));
    assert!(!dashboard.contains("id=\"wzHost\""));
    assert!(!dashboard.contains("id=\"wzUser\""));
    assert!(dashboard.contains(r#"[...ONBOARDING_STEPS,"activate"]"#));
    assert!(dashboard.contains("accepted.activates_inference"));
    assert!(dashboard.contains("run.expectsActivation ? \"activate\" : \"smoke\""));
    assert!(dashboard.contains("The producer and Mac decoder are ready on the same server"));
}

#[test]
fn dashboard_keeps_editorial_copy_out_of_the_work_surface() {
    let dashboard = std::fs::read_to_string(common::repo_root().join("ui/muser-dashboard.html"))
        .expect("ui asset");
    let main_start = dashboard.find("<main>").expect("main start");
    let main_end = dashboard.find("</main>").expect("main end");
    let work_surface = &dashboard[main_start..main_end];

    assert!(
        !work_surface.contains("<p"),
        "dashboard panels must not grow explanatory paragraphs"
    );
    for raw in work_surface
        .split('>')
        .skip(1)
        .filter_map(|rest| rest.split('<').next())
    {
        let text = raw.split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(
            text.chars().count() <= 64,
            "long work-surface copy belongs in About/help: {text}"
        );
    }
    for prose in [
        "one card per configured engine",
        "Runs the full add-node pipeline",
        "Bring a remote prefill node online from here",
        "That is what this panel's header chip is about",
        "read from the console's history store",
        "event persistence is explicitly outside v1",
        "release qualification seals them",
        "populate on first reuse or transfer",
        "Drawn from completed transfers and healthy nodes",
        "No positional rotation — every token stays visible",
        "Ring buffer over the newest 2048 tokens",
        "Topology — this process",
    ] {
        assert!(!dashboard.contains(prose), "verbose product copy: {prose}");
    }
    for required in [
        "id=\"aboutBtn\"",
        "id=\"topologyTitle\">Topology</h2>",
        "id=\"aboutBackdrop\"",
        "About dashboard data",
        "Live engine or console measurement.",
        "Events show the current snapshot only.",
        "The phone must trust this HTTPS address.",
        "Used only to authorize this action; never added to URLs or logs.",
    ] {
        assert!(
            dashboard.contains(required),
            "missing concise copy: {required}"
        );
    }
}

#[test]
fn dashboard_tabs_are_rationalized_mobile_safe_and_pair_without_markup() {
    let dashboard = std::fs::read_to_string(common::repo_root().join("ui/muser-dashboard.html"))
        .expect("ui asset");
    let between = |start: &str, end: &str| {
        let from = dashboard
            .find(start)
            .unwrap_or_else(|| panic!("missing {start}"));
        let to = dashboard[from..]
            .find(end)
            .map(|offset| from + offset)
            .unwrap_or_else(|| panic!("missing {end} after {start}"));
        &dashboard[from..to]
    };

    for tab in ["fleet", "inference", "activity", "history"] {
        assert!(dashboard.contains(&format!("id=\"tab-{tab}\"")));
        assert!(dashboard.contains(&format!("id=\"page-{tab}\" role=\"tabpanel\"")));
    }
    let fleet = between("id=\"page-fleet\"", "id=\"page-inference\"");
    assert!(fleet.contains("id=\"fleetCards\""));
    assert!(fleet.contains("id=\"nodes\""));
    let inference = between("id=\"page-inference\"", "id=\"page-activity\"");
    for id in [
        "chatPanel",
        "chatInput",
        "chatSend",
        "pipe",
        "ecCache",
        "kvNope",
        "tricks",
    ] {
        assert!(inference.contains(&format!("id=\"{id}\"")), "{id}");
    }
    let activity = between("id=\"page-activity\"", "id=\"page-history\"");
    for id in ["wRps", "sessRows", "evlog"] {
        assert!(activity.contains(&format!("id=\"{id}\"")), "{id}");
    }
    let history = between("id=\"page-history\"", "</main>");
    assert!(history.contains("id=\"histGrid\""));

    // The dashboard must respond to the space each panel actually receives,
    // not just jump between a desktop and phone viewport breakpoint.
    assert!(dashboard.contains("minmax(min(100%,max(410px,calc(50% - 8px))),1fr)"));
    assert!(dashboard.contains("container-type:inline-size"));
    assert!(dashboard.contains("@container (max-width:460px)"));
    assert!(dashboard.contains("content:attr(data-label)"));
    assert!(
        dashboard.contains("grid-template-columns:repeat(auto-fit,minmax(min(100%,300px),1fr))")
    );
    assert!(dashboard.contains("new ResizeObserver(entries=>"));
    assert!(dashboard.contains("requestAnimationFrame(resizeDashboardVisuals)"));
    assert!(!dashboard.contains("class=\"nodes two\""));
    assert!(dashboard.contains(
        "class=\"scroll-x\" tabindex=\"0\" role=\"region\" aria-label=\"Active sessions table\""
    ));
    assert!(dashboard.contains("role=\"tablist\""));
    assert!(dashboard.contains("event.key===\"ArrowRight\""));
    assert!(dashboard.contains("event.key===\"Enter\" || event.key===\" \""));
    assert!(dashboard.contains("id=\"pairBtn\" type=\"button\" hidden>Pair device</button>"));
    assert!(dashboard.contains("base+\"/v1/chat/completions\""));
    assert!(dashboard.contains(
        "if(typeof delta.reasoning_content===\"string\" && delta.reasoning_content){\n          if(ttftMs==null) ttftMs=performance.now()-startedAt;"
    ));
    assert!(dashboard.contains("if(!contentStarted){"));
    assert!(dashboard.contains(
        "#chatPanel{block-size:clamp(380px,44vh,430px);block-size:clamp(380px,44dvh,430px);"
    ));
    assert!(dashboard.contains(".chatwrap{display:flex;flex:1 1 auto;min-height:0;"));
    assert!(dashboard.contains(".chatlog{display:flex;flex:1 1 auto;min-height:0;"));
    assert!(dashboard.contains("overscroll-behavior:contain;scrollbar-gutter:stable"));
    assert!(!dashboard.contains("max-height:400px"));
    assert!(dashboard.contains("headers[\"x-csrf-token\"]=dashboardCsrf"));
    assert!(dashboard
        .contains("if(dashboardBearer) authHeaders.authorization=\"Bearer \"+dashboardBearer"));
    assert!(
        dashboard.contains("else if(dashboardCsrf) authHeaders[\"x-csrf-token\"]=dashboardCsrf")
    );
    assert!(!dashboard.contains("const authHeaders = location.protocol===\"https:\""));
    assert!(dashboard.contains("location.protocol===\"https:\" && fleetInstances"));
    assert!(!dashboard.contains(
        "async function restoreDashboardSession(){\n  if(location.protocol!==\"https:\")"
    ));

    let capture = dashboard
        .find("if(location.hash.startsWith(\"#pair=\"))")
        .unwrap();
    let strip = dashboard[capture..]
        .find("history.replaceState")
        .map(|offset| capture + offset)
        .unwrap();
    let first_fetch = dashboard.find("fetch(").unwrap();
    assert!(
        strip < first_fetch,
        "pairing fragment must be stripped before network I/O"
    );
    let pairing_js = between(
        "/* ============================================================= DEVICE PAIRING */",
        "const root=document.documentElement",
    );
    assert!(!pairing_js.contains("innerHTML"));
    assert!(pairing_js.contains("document.createElementNS"));
    assert!(pairing_js.contains("pairQr.replaceChildren"));

    // Fleet's instantaneous agent cards consume the native history fetch
    // even while the chart tab is hidden. Only painting is tab-gated.
    assert!(dashboard
        .contains("function histActive(){ return !!(fleetInstances && selectedInstance); }"));
    assert!(dashboard.contains("if(histPanel.hidden || activeTab!==\"history\") return;"));
}

#[tokio::test]
async fn healthz_needs_no_auth() {
    let console = console_only().await;
    let client = common::client();
    let (parts, body) = common::request(&client, "GET", console, "/healthz", &[], b"").await;
    assert_eq!(parts.status, 200);
    assert_eq!(body.as_ref(), br#"{"ok":true}"#);
}

#[tokio::test]
async fn login_over_http_gets_engine_exact_tls_required() {
    let console = console_only().await;
    let client = common::client();
    let (parts, body) =
        common::request(&client, "POST", console, "/v1/dashboard/login", &[], b"").await;
    assert_eq!(parts.status, 400);
    assert_eq!(
        body.as_ref(),
        common::engine_error_body(
            "tls_required",
            "dashboard sessions require HTTPS; use bearer authentication on loopback HTTP"
        )
    );
}

#[tokio::test]
async fn no_other_file_is_reachable() {
    let console = console_only().await;
    let client = common::client();
    for path in [
        "/../PROVENANCE",
        "/%2e%2e/PROVENANCE",
        "/ui/muser-dashboard.html",
        "/muser-dashboard.html",
        "/dashboard/../schema/metrics-schema.json",
        "/favicon.ico",
    ] {
        let (parts, body) = common::request(&client, "GET", console, path, &[], b"").await;
        assert_eq!(parts.status, 404, "GET {path} must not resolve");
        // Engine parity: the engine registers no fallback, so unknown routes
        // get axum's default empty-body 404.
        assert!(
            body.is_empty(),
            "GET {path} must get the engine's empty 404 body"
        );
    }
}
