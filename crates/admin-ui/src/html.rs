//! The server-rendered dashboard (maud).
//!
//! One page at `/`, refreshed in place by HTMX every 5 seconds: HTMX re-`GET`s
//! `/`, selects the `#dashboard` fragment out of the response, and swaps it.
//! Plain HTML + inline CSS; no build step, no client framework.

use maud::{DOCTYPE, Markup, PreEscaped, html};

use crate::costs::CostsReport;
use crate::journal::SessionSummary;
use crate::receipts::ReceiptSummary;
use crate::trust::{ReceiptVerification, WalletResponse};

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

/// The capability-wallet card list.
fn wallet_grants(wallet: &WalletResponse) -> Markup {
    html! {
        div.feed {
            @if wallet.grants.is_empty() {
                div.empty { "No active capability grants tracked." }
            } @else {
                @for g in &wallet.grants {
                    div.receipt {
                        span.r-provider { (g.subject) }
                        span.r-tokens { (g.audience) }
                        @if !g.tools.is_empty() {
                            span.r-tools { (g.tools.join(", ")) }
                        }
                        span.r-cost { (g.budget_remaining) " ¢ remaining" }
                        span.r-id { "expires " (g.expires_unix) }
                    }
                }
            }
        }
    }
}

/// The receipt-chain verification status line.
fn chain_status(v: &ReceiptVerification) -> Markup {
    html! {
        @if v.chain_valid {
            div.chain-ok { "✓ " (v.receipt_count) " receipts, chain valid" }
        } @else {
            div.chain-bad {
                "✗ broken at receipt index " (v.error_index.unwrap_or(0))
                @if let Some(reason) = &v.reason {
                    ": " (reason)
                }
            }
        }
    }
}

/// The Cedar policy debugger — a form posting to `/api/trust/policy/debug`
/// via HTMX, rendering the raw JSON decision into a result pane. When no
/// policy bundle is configured (`--policy-bundle` was not passed), shows a
/// note instead of the form.
fn policy_debugger(policies_configured: bool) -> Markup {
    html! {
        @if policies_configured {
            form
                hx-get="/api/trust/policy/debug"
                hx-target="#policy-result"
                hx-swap="innerHTML"
            {
                div.debugger-row {
                    input type="text" name="principal" placeholder="Principal, e.g. User::\"alice\"" required;
                    input type="text" name="action" placeholder="Action, e.g. Action::\"Submit\"" required;
                    input type="text" name="resource" placeholder="Resource, e.g. Session::\"s1\"" required;
                    button type="submit" { "Evaluate" }
                }
            }
            pre #policy-result {}
        } @else {
            div.empty { "Policy debugger not configured — start ardur-admin with --policy-bundle <path> to enable it." }
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

/// The Trust Center section. Deliberately rendered as a **sibling** of the
/// auto-refreshing `#dashboard` fragment, not nested inside it: the policy
/// debugger is an interactive form, and `#dashboard`'s `hx-trigger="every
/// 5s"` poll replaces its entire subtree on each refresh — nesting the form
/// there would silently wipe whatever an operator was mid-typing every five
/// seconds. The receipt-chain status and capability wallet are static
/// snapshots-at-page-load here rather than live-refreshing, which is the
/// right tradeoff for a form that shares the section.
fn trust_center(
    wallet: &WalletResponse,
    chain: &ReceiptVerification,
    policies_configured: bool,
) -> Markup {
    html! {
        section {
            h2 { "Trust Center" }
            h3 { "Receipt chain" }
            (chain_status(chain))
            h3 { "Capability wallet" }
            (wallet_grants(wallet))
            h3 { "Policy debugger" }
            (policy_debugger(policies_configured))
        }
    }
}

/// One approval card, rendered generically from the opaque JSON
/// `ardur-server` returns (admin-ui deliberately does not redeclare
/// `ApprovalCard` as a typed struct, so it cannot silently drift from
/// ardur-server's authoritative shape). `id` and `status` get dedicated
/// treatment; every other field is dumped as a compact `key: value` list so
/// the view stays useful as the card schema evolves upstream.
fn approval_card(card: &serde_json::Value) -> Markup {
    let id = card.get("id").and_then(|v| v.as_str()).unwrap_or("?");
    let status = card
        .get("status")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");
    html! {
        div.receipt {
            span.r-id title=(id) { (short(id)) }
            span.r-provider { (status) }
            @if let Some(object) = card.as_object() {
                @for (k, v) in object {
                    @if k != "id" && k != "status" {
                        span.r-tools { (k) ": " (v.to_string()) }
                    }
                }
            }
            @if status == "pending" {
                button.approve-btn
                    hx-post=(format!("/api/operator/approvals/{id}/approve"))
                    hx-swap="none"
                    hx-confirm="Approve this action?"
                { "Approve" }
                button.reject-btn
                    hx-post=(format!("/api/operator/approvals/{id}/reject"))
                    hx-swap="none"
                    hx-confirm="Reject this action?"
                { "Reject" }
            }
        }
    }
}

/// The approvals-list fragment `/operator/approvals` returns — either the
/// rendered cards, an empty state, or (if the proxied call to ardur-server
/// itself failed) an inline error. Loaded via `hx-trigger="load,
/// approvalsChanged from:body"`: once on page load, and again whenever an
/// approve/reject action fires the `approvalsChanged` event (see
/// [`approval_card`]'s buttons and the `HX-Trigger` response header the
/// decide routes set on success) — not on a fixed timer, so a slow or
/// unreachable ardur-server cannot repeatedly stall the whole dashboard the
/// way nesting it in `#dashboard`'s 5-second poll would.
pub fn approvals_list_fragment(cards: Result<&[serde_json::Value], &str>) -> Markup {
    html! {
        @match cards {
            Ok(cards) if cards.is_empty() => div.empty { "No approval cards." }
            Ok(cards) => div.feed {
                @for card in cards {
                    (approval_card(card))
                }
            }
            Err(message) => div.chain-bad { "Could not reach ardur-server: " (message) }
        }
    }
}

/// The Approvals section shell — a sibling of the auto-refreshing
/// `#dashboard` fragment and of the Trust Center section, for the same
/// reason as the policy debugger (approve/reject actions must not be wiped
/// by an unrelated timed poll) plus one more: fetching the list is a network
/// call to another process, and embedding it in a 5-second poll would mean
/// a slow/unreachable ardur-server stalls the *entire* dashboard every five
/// seconds. When the proxy isn't configured, shows a note instead of the
/// (would-503) list container.
fn approvals_section(configured: bool) -> Markup {
    html! {
        section {
            h2 { "Approvals" }
            @if configured {
                div #approvals-list
                    hx-get="/operator/approvals"
                    hx-trigger="load, approvalsChanged from:body"
                    hx-swap="innerHTML"
                {
                    div.empty { "Loading…" }
                }
            } @else {
                div.empty { "Approvals proxy not configured — start ardur-admin with --server-url <url> --server-admin-token <token> to enable it." }
            }
        }
    }
}

/// The full page.
pub fn dashboard(
    report: &CostsReport,
    sessions: &[SessionSummary],
    receipts: &[ReceiptSummary],
    wallet: &WalletResponse,
    chain: &ReceiptVerification,
    policies_configured: bool,
    approvals_configured: bool,
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
                    span.subtitle { "observability + operator console · auto-refreshing" }
                }
                main {
                    (dashboard_fragment(report, sessions, receipts))
                    (approvals_section(approvals_configured))
                    (trust_center(wallet, chain, policies_configured))
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
h3 { font-size: .85rem; margin: 1rem 0 .5rem; color: #8888; }
.chain-ok { color: #2e9e5b; font-size: .9rem; }
.chain-bad { color: #d94a4a; font-size: .9rem; }
.debugger-row { display: flex; flex-wrap: wrap; gap: .5rem; }
.debugger-row input { flex: 1 1 160px; padding: .4rem .5rem; border: 1px solid #8884; border-radius: 6px; background: transparent; color: inherit; }
.debugger-row button { padding: .4rem 1rem; border: 1px solid #4a90d9; border-radius: 6px; background: #4a90d9; color: white; cursor: pointer; }
#policy-result:not(:empty) { display: block; margin-top: .75rem; padding: .75rem; border: 1px solid #8883; border-radius: 8px; font-size: .8rem; white-space: pre-wrap; word-break: break-all; }
.approve-btn, .reject-btn { padding: .3rem .8rem; border-radius: 6px; cursor: pointer; font-size: .8rem; margin-left: .4rem; }
.approve-btn { border: 1px solid #2e9e5b; background: #2e9e5b; color: white; }
.reject-btn { border: 1px solid #d94a4a; background: transparent; color: #d94a4a; }
"#;
