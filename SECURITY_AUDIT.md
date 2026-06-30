# ardur-agent Deep Security & Code Audit Report

**Date:** 2026-06-17
**Auditor:** Hermes Agent
**Scope:** Full codebase (crates/)
**Branch:** dev

---

## 🚨 CRITICAL FINDINGS

### 1. Hardcoded API Keys in Test Code (MEDIUM)
**Location:** `crates/provider-openai-compat/src/lib.rs`, `crates/provider-openrouter/src/lib.rs`
**Issue:** Test code contains hardcoded API key patterns (`"sk-ope...cret"`)
**Risk:** Could accidentally be used in production; reveals key format
**Fix:** Use `env!(...)` or mock values that don't resemble real keys

### 2. 364 unwrap() Calls in Non-Test Code (HIGH)
**Locations:** Throughout codebase, especially:
- `crates/hooks-openclaw-compat/src/hook.rs:97-98`
- `crates/embeddings/src/lib.rs:229, 240, 273, 278`
- `crates/tool-registry/src/skills/tool.rs:224-261`
**Issue:** `unwrap()` causes panic on error — production crash vector
**Risk:** Denial of service via crafted inputs
**Fix:** Replace with `?` error propagation or `match` with graceful handling

### 3. 8 panic! Calls in Non-Test Code (HIGH)
**Locations:**
- `crates/provider-openai-compat/src/lib.rs:903`
- `crates/provider-openai-compat/src/streaming.rs:637`
- `crates/provider-claude-cli/src/lib.rs:928, 963`
- `crates/browser/src/lib.rs:115`
- `crates/provider-selector/src/lib.rs:338`
- `crates/provider-openrouter/src/lib.rs:890`
- `crates/provider-openrouter/src/streaming.rs:650`
**Issue:** `panic!` aborts the entire process
**Risk:** Complete service outage from unexpected input
**Fix:** Return `Result::Err` with descriptive error

### 4. Timing Attack in Admin UI Auth (HIGH)
**Location:** `crates/admin-ui/src/auth.rs:45`
**Code:** `header_value.as_bytes() == self.expected_header.as_bytes()`
**Issue:** Byte-by-byte comparison leaks timing information
**Risk:** Brute-force attack on bearer token via timing analysis
**Fix:** Use `constant_time_eq` from `subtle` crate

### 5. 81 expect() Calls (MEDIUM)
**Locations:** Throughout, especially:
- `crates/webhook/src/signature.rs:37` (HMAC init)
- `crates/channel-telegram/src/lib.rs:91-150`
- `crates/tool-registry/src/skills/skill.rs:164-232`
**Issue:** `expect()` panics with custom message — crashes production
**Fix:** Convert to proper error handling with `Result`

---

## ⚠️ HIGH SEVERITY FINDINGS

### 6. Potential Lock-Across-Await Deadlocks
**Locations:**
- `crates/channel-telegram/src/channel.rs:243` (`inbound_rx.lock().await`)
- `crates/channel-discord/src/channel.rs:173` (`client.lock().await`)
**Issue:** Holding a lock across `.await` can cause deadlocks in async code
**Fix:** Use `tokio::sync::Mutex` (already used) but ensure lock is dropped before await

### 7. Shell Command Execution Tool (HIGH)
**Location:** `crates/tool-registry/src/builtins/shell.rs:6`
**Issue:** "executes arbitrary commands through the shell"
**Risk:** Remote code execution if tool registry is compromised
**Mitigation:** Already gated by cap-token/Cedar, but needs sandboxing

### 8. unsafe Blocks in Tests (LOW)
**Locations:**
- `crates/memory-qdrant/tests/config_env.rs:20, 33, 48`
- `crates/server/tests/boot_hybrid.rs:32`
- `crates/server/tests/config_from_env.rs:58, 63`
- `crates/provider-selector/tests/selector_env.rs:27, 38`
**Issue:** `unsafe { std::env::set_var(...) }` — thread-unsafe in tests
**Risk:** Test flakiness, potential data races
**Fix:** Use scoped env vars or serial_test crate

### 9. Problematic Terminology (MEDIUM)
**Locations:**
- `crates/tool-registry/src/echo.rs:2` — "sanity demo"
- `crates/cedar-policy/tests/smoke.rs:5-6` — "Dummy" struct
- `crates/server/tests/config_from_env.rs:140` — "Sanity"
- `crates/e2e-tests/tests/scenario_06_multi_agent_attenuation.rs:116` — "Sanity"
**Issue:** Non-inclusive terminology
**Fix:** Replace with "validation check", "placeholder", "health check"

---

## ℹ️ MEDIUM SEVERITY FINDINGS

### 10. 96 Crates Missing Top-Level Documentation
**Issue:** No `//!` module documentation in many source files
**Impact:** Poor maintainability, onboarding difficulty
**Fix:** Add module-level docs explaining purpose and usage

### 11. Telegram Test Token (LOW)
**Location:** `crates/channel-telegram/src/lib.rs:76`
**Issue:** Test uses fake token format `"123456:ABC-DEF..."`
**Risk:** Could confuse developers about real token format
**Fix:** Clearly mark as test-only with `#[cfg(test)]`

### 12. CORS Wildcard in Tests (LOW)
**Location:** `crates/tool-registry/tests/http_fetch.rs:137`
**Issue:** Test for wildcard subdomain matching
**Risk:** Not production code, but tests should reflect secure defaults
**Fix:** Ensure production code rejects wildcards by default

### 13. Missing Dependency Audit Tools
**Issue:** `cargo-audit`, `cargo-license`, `cargo-outdated` not installed
**Impact:** Can't detect known CVEs, license conflicts, stale dependencies
**Fix:** Install and run regularly in CI

---

## ✅ POSITIVE FINDINGS

### Security Strengths
1. **Cap-token/Cedar enforcement** — Proper authorization framework
2. **Receipt chaining** — Audit trail for all operations
3. **Cost gating** — Budget controls prevent runaway spending
4. **URL validation** — HTTPS/loopback enforcement in providers
5. **API key redaction** — Debug impls don't leak secrets
6. **Path traversal blocking** — Canonicalization and `../` rejection
7. **Test coverage** — 59% test-to-source ratio is good
8. **No wildcard CORS** in production code
9. **No debug mode** enabled by default
10. **No weak credentials** in production paths

---

## 📋 RECOMMENDED FIXES (Priority Order)

### P0 (Fix Immediately)
1. Replace `==` with `constant_time_eq` in `admin-ui/src/auth.rs:45`
2. Remove all `panic!` from non-test production code (8 locations)
3. Replace `unwrap()` in production paths with `?` or `match` (start with top 20)

### P1 (Fix This Week)
4. Replace `expect()` in production code with proper error handling
5. Fix lock-across-await patterns in channel code
6. Add sandboxing to shell execution tool
7. Replace problematic terminology

### P2 (Fix This Sprint)
8. Add top-level documentation to all crates
9. Install and configure `cargo-audit` in CI
10. Add property-based tests for cost arithmetic
11. Add accessibility documentation

### P3 (Ongoing)
12. Reduce `unwrap()` count from 364 to <50
13. Add `cargo-outdated` to CI pipeline
14. Add license compliance check
15. Add timing attack tests to CI

---

## 🔧 Quick Fixes You Can Apply Now

```bash
# Fix 1: Install cargo-audit
cargo install cargo-audit

# Fix 2: Run audit
cargo audit

# Fix 3: Check for new unwrap() additions
grep -r "unwrap()" crates/ --include="*.rs" | grep -v test | wc -l

# Fix 4: Find all expect() calls
grep -r "expect(\"" crates/ --include="*.rs" | grep -v test

# Fix 5: Find all panic! calls
grep -r "panic!" crates/ --include="*.rs" | grep -v test
```

---

## 📊 Audit Summary

| Category | Count | Severity |
|----------|-------|----------|
| unwrap() in production | 364 | HIGH |
| panic! in production | 8 | HIGH |
| expect() in production | 81 | MEDIUM |
| Hardcoded secrets | 2 | MEDIUM |
| Timing attacks | 1 | HIGH |
| Deadlock risks | 2 | HIGH |
| unsafe blocks | 8 | LOW |
| Problematic terms | 4 | MEDIUM |
| Missing docs | 96 | LOW |
| Test coverage | 59% | GOOD |

**Overall Security Posture:** GOOD with concerning patterns
**Recommendation:** Address P0 and P1 items before next release

---

*Report generated by Hermes Agent automated audit*
*Methodology: Static analysis, pattern matching, best-practice verification*
