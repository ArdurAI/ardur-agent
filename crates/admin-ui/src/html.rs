//! The server-rendered dashboard (maud).
//!
//! One page at `/`, refreshed in place by HTMX every 5 seconds: HTMX re-`GET`s
//! `/`, selects the `#dashboard` fragment out of the response, and swaps it.
//! Plain HTML + inline CSS; no build step, no client framework.

use maud::{DOCTYPE, Markup, PreEscaped, html};

use crate::costs::CostsReport;
use crate::journal::SessionSummary;
use crate::receipts::ReceiptSummary;

/// HTMX 1.9 from a CDN (the dashboard's only client-side dependency).
const HTMX_CDN: &str = "https://unpkg.com/htmx.org@1.9.12";

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
                }
                main {
                    (dashboard_fragment(report, sessions, receipts))
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
"#;
