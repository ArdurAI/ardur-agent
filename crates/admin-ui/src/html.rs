//! The server-rendered dashboard (maud).
//!
//! One page at `/`, refreshed in place by HTMX every 5 seconds: HTMX re-`GET`s
//! `/`, selects the `#dashboard` fragment out of the response, and swaps it.
//! Plain HTML + inline CSS; no build step, no client framework.

use maud::{DOCTYPE, Markup, PreEscaped, html};

use crate::costs::CostsReport;
use crate::journal::SessionSummary;
use crate::receipts::ReceiptSummary;
use crate::security_events::{SecurityEvent, SecurityEventView};
use crate::trust::{ChainOverview, WalletResponse};

/// HTMX 1.9 from a CDN (the dashboard's only client-side dependency).
const HTMX_CDN: &str = "https://unpkg.com/htmx.org@1.9.12";

/// Format a millisecond epoch as a compact `YYYY-MM-DD HH:MM:SSZ` UTC string,
/// falling back to the raw value when it is out of representable range.
fn utc_ms(ms: u64) -> String {
    chrono::DateTime::<chrono::Utc>::from_timestamp_millis(ms as i64)
        .map(|dt| dt.format("%Y-%m-%d %H:%M:%SZ").to_string())
        .unwrap_or_else(|| format!("ms:{ms}"))
}

/// The shared top navigation, marking `active` on the current page.
fn nav(active: &str) -> Markup {
    html! {
        nav.tabs {
            a.tab.active[active == "dashboard"] href="/" { "Dashboard" }
            a.tab.active[active == "trust"] href="/trust" { "Trust Center" }
        }
    }
}

/// Render cents as a `$x.yy` dollar string.
fn dollars(cents: u64) -> String {
    format!("${}.{:02}", cents / 100, cents % 100)
}

/// Truncate an id for compact display, keeping it copy-selectable in full via
/// the `title` attribute at the call site.
fn short(id: &str) -> String {
    if id.len() > 12 {
        format!("{}…", &id[..12])
    } else {
        id.to_string()
    }
}

/// A summary cost card.
fn card(label: &str, cents: u64) -> Markup {
    html! {
        div.card {
            div.card-label { (label) }
            div.card-value { (dollars(cents)) }
        }
    }
}

/// The cost-by-provider horizontal bar chart.
fn provider_bars(report: &CostsReport) -> Markup {
    let max = report
        .by_provider
        .iter()
        .map(|p| p.cents)
        .max()
        .unwrap_or(0);
    html! {
        div.bars {
            @if report.by_provider.is_empty() {
                div.empty { "No receipts yet." }
            } @else {
                @for p in &report.by_provider {
                    @let pct = (p.cents * 100).checked_div(max).map_or(0, |v| v.max(2));
                    div.bar-row {
                        span.bar-label title=(p.provider) { (short(&p.provider)) }
                        span.bar-track {
                            span.bar-fill style=(format!("width:{pct}%")) {}
                        }
                        span.bar-value { (dollars(p.cents)) " · " (p.count) }
                    }
                }
            }
        }
    }
}

/// The recent-sessions table.
fn sessions_table(sessions: &[SessionSummary]) -> Markup {
    html! {
        table {
            thead {
                tr { th { "Session" } th { "Messages" } th { "Entries" } th { "Last cost" } th { "Last activity (ms)" } }
            }
            tbody {
                @if sessions.is_empty() {
                    tr { td colspan="5" .empty { "No sessions yet." } }
                } @else {
                    @for s in sessions {
                        tr {
                            td title=(s.id) { code { (short(&s.id)) } }
                            td { (s.message_count) }
                            td { (s.entry_count) }
                            td { (s.last_cost_cents.map(dollars).unwrap_or_else(|| "—".into())) }
                            td { (s.last_activity_ms.map(|m| m.to_string()).unwrap_or_else(|| "—".into())) }
                        }
                    }
                }
            }
        }
    }
}

/// The recent-receipts feed.
fn receipts_feed(receipts: &[ReceiptSummary]) -> Markup {
    html! {
        div.feed {
            @if receipts.is_empty() {
                div.empty { "No receipts yet." }
            } @else {
                @for r in receipts {
                    div.receipt {
                        span.r-provider { (r.provider) }
                        span.r-cost { (dollars(r.cents)) }
                        span.r-tokens { (r.tokens_in) "→" (r.tokens_out) " tok" }
                        @if r.tool_call_count > 0 {
                            span.r-tools { (r.tool_call_count) " tool(s): " (r.tool_calls.join(", ")) }
                        }
                        span.r-id title=(r.receipt_id) { (short(&r.receipt_id)) }
                    }
                }
            }
        }
    }
}

/// The inner, refreshable dashboard fragment.
fn dashboard_fragment(
    report: &CostsReport,
    sessions: &[SessionSummary],
    receipts: &[ReceiptSummary],
) -> Markup {
    html! {
        div #dashboard
            hx-get="/"
            hx-trigger="every 5s"
            hx-select="#dashboard"
            hx-swap="outerHTML"
        {
            section.cards {
                (card("Today", report.today_cents))
                (card("Last 7 days", report.last_7d_cents))
                (card("Last 30 days", report.last_30d_cents))
                (card("All time", report.total_cents))
            }

            section {
                h2 { "Cost by provider" }
                (provider_bars(report))
            }

            section {
                h2 { "Recent sessions" }
                (sessions_table(sessions))
            }

            section {
                h2 { "Recent receipts" }
                (receipts_feed(receipts))
            }
        }
    }
}

/// The full page.
pub fn dashboard(
    report: &CostsReport,
    sessions: &[SessionSummary],
    receipts: &[ReceiptSummary],
) -> Markup {
    html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                title { "ardur-admin" }
                script src=(HTMX_CDN) {}
                style { (PreEscaped(STYLE)) }
            }
            body {
                header.topbar {
                    h1 { "ardur-admin" }
                    span.subtitle { "read-only observability · auto-refreshing" }
                    (nav("dashboard"))
                }
                main {
                    (dashboard_fragment(report, sessions, receipts))
                }
            }
        }
    }
}

/// The receipt-chain verification banner: green when every link holds, red with
/// the offending index when a `parent_hash` breaks.
fn chain_banner(overview: &ChainOverview) -> Markup {
    html! {
        @if overview.chain_valid {
            div.banner.ok {
                strong { "Chain verified" }
                " · " (overview.total) " receipt(s), every parent-hash link intact."
            }
        } @else {
            div.banner.bad {
                strong { "Chain broken" }
                @if let Some(idx) = overview.error_index {
                    " · first mismatch at index " (idx) " of " (overview.total) "."
                } @else {
                    " · " (overview.total) " receipt(s)."
                }
            }
        }
    }
}

/// The receipt-chain explorer table (newest first).
fn chain_table(overview: &ChainOverview) -> Markup {
    html! {
        table {
            thead {
                tr {
                    th { "#" } th { "Verb" } th { "Provider" } th { "Cost" }
                    th { "Tokens" } th { "Tools" } th { "Link" } th { "Issued (UTC)" } th { "Receipt" }
                }
            }
            tbody {
                @if overview.links.is_empty() {
                    tr { td colspan="9" .empty { "No receipts yet." } }
                } @else {
                    @for l in &overview.links {
                        tr {
                            td { (l.index) }
                            td { code { (l.verb) } }
                            td { (l.provider) }
                            td { (dollars(l.cents)) }
                            td { (l.tokens_in) "→" (l.tokens_out) }
                            td { (l.tool_count) }
                            td {
                                @if l.link_ok { span.pill.ok { "ok" } }
                                @else { span.pill.bad { "broken" } }
                            }
                            td { (utc_ms(l.issued_at_ms)) }
                            td title=(l.receipt_id) { code { (short(&l.receipt_id)) } }
                        }
                    }
                }
            }
        }
    }
}

/// The capability-wallet table: active (non-expired) grants.
fn wallet_table(wallet: &WalletResponse) -> Markup {
    html! {
        table {
            thead {
                tr {
                    th { "Token" } th { "Subject" } th { "Audience" }
                    th { "Tools" } th { "Budget" } th { "Expires (UTC)" }
                }
            }
            tbody {
                @if wallet.grants.is_empty() {
                    tr { td colspan="6" .empty { "No active capability grants." } }
                } @else {
                    @for g in &wallet.grants {
                        tr {
                            td title=(g.token_id) { code { (short(&g.token_id)) } }
                            td { (g.subject) }
                            td { (g.audience) }
                            td { (g.tools.join(", ")) }
                            td { (g.budget_remaining) }
                            td { (utc_ms(g.expires_unix.saturating_mul(1000))) }
                        }
                    }
                }
            }
        }
    }
}

/// The cost-ledger by-day table (most recent day first).
fn ledger_table(report: &CostsReport) -> Markup {
    html! {
        table {
            thead {
                tr { th { "Day (UTC)" } th { "Receipts" } th { "Cost" } }
            }
            tbody {
                @if report.by_day.is_empty() {
                    tr { td colspan="3" .empty { "No settled cost yet." } }
                } @else {
                    @for d in &report.by_day {
                        tr {
                            td { (d.day) }
                            td { (d.count) }
                            td { (dollars(d.cents)) }
                        }
                    }
                }
            }
        }
    }
}

/// The per-gate summary chips for the security-event panels.
fn gate_chips(events: &SecurityEventView) -> Markup {
    html! {
        div.chips {
            @if events.by_gate.is_empty() {
                span.chip { "no events" }
            } @else {
                @for g in &events.by_gate {
                    span.chip { (g.gate) " · " (g.count) }
                }
            }
        }
    }
}

/// The policy/gate decision table (non-injection denials), newest first.
fn decisions_table(events: &[SecurityEvent]) -> Markup {
    html! {
        table {
            thead {
                tr { th { "When (UTC)" } th { "Gate" } th { "Decision" } th { "Reason" } }
            }
            tbody {
                @if events.is_empty() {
                    tr { td colspan="4" .empty { "No gate denials recorded." } }
                } @else {
                    @for e in events {
                        tr {
                            td { (utc_ms(e.at_ms)) }
                            td { span.pill.bad { (e.gate) } }
                            td { (e.decision.as_deref().unwrap_or("deny")) }
                            td { (e.reason.as_deref().unwrap_or("—")) }
                        }
                    }
                }
            }
        }
    }
}

/// The injection-event table, newest first. Renders flag pattern/category/
/// confidence — never matched text (the writer already stripped it).
fn injection_table(events: &[SecurityEvent]) -> Markup {
    html! {
        table {
            thead {
                tr { th { "When (UTC)" } th { "Filter" } th { "Flags (pattern · class · conf)" } }
            }
            tbody {
                @if events.is_empty() {
                    tr { td colspan="3" .empty { "No injection blocks recorded." } }
                } @else {
                    @for e in events {
                        tr {
                            td { (utc_ms(e.at_ms)) }
                            td { code { (e.filter_id.as_deref().unwrap_or("—")) } }
                            td {
                                @if e.flags.is_empty() {
                                    "—"
                                } @else {
                                    @for f in &e.flags {
                                        span.pill.bad {
                                            (f.pattern_id) " · " (f.category) " · "
                                            (format!("{:.2}", f.confidence))
                                        }
                                        " "
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// The Trust Center page: receipt-chain integrity + explorer, the cost ledger,
/// the capability wallet, the policy-decision log, and the injection-event feed.
/// Every panel is a read-only projection of on-disk artifacts and boot-time
/// configured state.
pub fn trust_center(
    overview: &ChainOverview,
    wallet: &WalletResponse,
    report: &CostsReport,
    events: &SecurityEventView,
    policy_enabled: bool,
) -> Markup {
    html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                title { "ardur-admin · Trust Center" }
                script src=(HTMX_CDN) {}
                style { (PreEscaped(STYLE)) }
            }
            body {
                header.topbar {
                    h1 { "ardur-admin" }
                    span.subtitle { "Trust Center · receipts · cost · policy" }
                    (nav("trust"))
                }
                main {
                    section.cards {
                        (card("Today", report.today_cents))
                        (card("Last 7 days", report.last_7d_cents))
                        (card("Last 30 days", report.last_30d_cents))
                        (card("All time", report.total_cents))
                    }

                    section {
                        h2 { "Receipt chain" }
                        (chain_banner(overview))
                        (chain_table(overview))
                    }

                    section {
                        h2 { "Cost ledger" }
                        (ledger_table(report))
                    }

                    section {
                        h2 { "Capability wallet" }
                        (wallet_table(wallet))
                    }

                    section {
                        h2 { "Policy decisions" }
                        @if policy_enabled {
                            p.note {
                                "A Cedar policy bundle is loaded. Trace a hypothetical decision via "
                                code { "GET /api/trust/policy/debug?principal=&action=&resource=" }
                                " — allow/deny with the matched policy ids."
                            }
                        } @else {
                            p.note { "No Cedar policy bundle configured (policy debugger disabled)." }
                        }
                        @if events.enabled {
                            (gate_chips(events))
                            (decisions_table(&events.decisions))
                        } @else {
                            p.note {
                                "Security-event log not configured — pass "
                                code { "--security-events <data>/security-events.jsonl" }
                                " to surface the recorded gate denials here."
                            }
                        }
                    }

                    section {
                        h2 { "Injection events" }
                        @if events.enabled {
                            (injection_table(&events.injection))
                        } @else {
                            p.note {
                                "Security-event log not configured — pass "
                                code { "--security-events <data>/security-events.jsonl" }
                                " to surface blocked injection attempts here."
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Inline stylesheet — kept small and dependency-free.
const STYLE: &str = r#"
:root { color-scheme: light dark; }
* { box-sizing: border-box; }
body { font-family: ui-sans-serif, system-ui, -apple-system, sans-serif; margin: 0; line-height: 1.5; }
.topbar { padding: 1rem 1.5rem; border-bottom: 1px solid #8884; display: flex; align-items: baseline; gap: 1rem; }
.topbar h1 { margin: 0; font-size: 1.25rem; }
.subtitle { color: #8888; font-size: .85rem; }
main { padding: 1.5rem; max-width: 1100px; margin: 0 auto; }
section { margin-bottom: 2rem; }
h2 { font-size: 1rem; text-transform: uppercase; letter-spacing: .05em; color: #8888; }
.cards { display: grid; grid-template-columns: repeat(auto-fit, minmax(150px, 1fr)); gap: 1rem; }
.card { border: 1px solid #8884; border-radius: 10px; padding: 1rem; }
.card-label { font-size: .8rem; color: #8888; }
.card-value { font-size: 1.6rem; font-weight: 600; }
table { width: 100%; border-collapse: collapse; font-size: .9rem; }
th, td { text-align: left; padding: .5rem .6rem; border-bottom: 1px solid #8883; }
th { font-weight: 600; color: #8888; }
code { font-family: ui-monospace, monospace; }
.bars { display: flex; flex-direction: column; gap: .4rem; }
.bar-row { display: grid; grid-template-columns: 160px 1fr 140px; align-items: center; gap: .6rem; font-size: .85rem; }
.bar-track { background: #8882; border-radius: 6px; height: 14px; overflow: hidden; }
.bar-fill { display: block; height: 100%; background: #4a90d9; }
.bar-value { text-align: right; color: #8888; }
.feed { display: flex; flex-direction: column; gap: .4rem; }
.receipt { display: flex; flex-wrap: wrap; gap: .75rem; padding: .5rem .6rem; border: 1px solid #8883; border-radius: 8px; font-size: .85rem; align-items: center; }
.r-provider { font-weight: 600; }
.r-cost { color: #4a90d9; }
.r-id { margin-left: auto; color: #8888; font-family: ui-monospace, monospace; }
.empty { color: #8888; font-style: italic; padding: .5rem 0; }
.tabs { margin-left: auto; display: flex; gap: .25rem; }
.tab { text-decoration: none; color: inherit; padding: .35rem .75rem; border-radius: 8px; font-size: .9rem; border: 1px solid transparent; }
.tab:hover { border-color: #8884; }
.tab.active { background: #4a90d922; border-color: #4a90d9; color: #4a90d9; font-weight: 600; }
.banner { padding: .6rem .8rem; border-radius: 8px; margin-bottom: .8rem; font-size: .9rem; }
.banner.ok { background: #2ecc7122; border: 1px solid #2ecc71; }
.banner.bad { background: #e74c3c22; border: 1px solid #e74c3c; }
.pill { padding: .1rem .5rem; border-radius: 999px; font-size: .75rem; font-weight: 600; }
.pill.ok { background: #2ecc7122; color: #2ecc71; }
.pill.bad { background: #e74c3c22; color: #e74c3c; }
.note { color: #8888; font-size: .9rem; }
.note code { background: #8882; padding: .1rem .35rem; border-radius: 4px; }
.chips { display: flex; flex-wrap: wrap; gap: .4rem; margin: .4rem 0 .8rem; }
.chip { padding: .2rem .6rem; border-radius: 999px; background: #8882; font-size: .8rem; }
"#;
