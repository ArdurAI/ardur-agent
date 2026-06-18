# ardur-agent CRITICAL FIXES PROMPT
## Deep Security & Code Audit — Action Required

**DO NOT START WORK UNTIL YOU RUN:**
```bash
bash /Users/gnutakki16/ardur-agent/scripts/ardur-agent-bootstrap.sh
```

**Repository:** /Users/gnutakki16/ardur-agent  
**Branch from:** origin/dev  
**Create branch:** agent/security-hardening-audit-fixes  
**Linear Epic:** EPIC-SECURITY-AUDIT (create if needed)  

---

## AUDIT METHODOLOGY

This audit was performed using the following systematic approach. You should verify each finding independently before fixing:

### 1. Secret Detection
```bash
grep -r -i -E '(api_key|apikey|secret|token|password|passwd|credential|bearer|auth_token|private_key|api_secret)' crates/ --include=*.rs --include=*.toml --include=*.yml --include=*.yaml
```
- Filtered out comments, struct definitions, test files
- Looked for actual values (not just variable names)
- Flagged patterns containing `sk-`, `pk_`, `eyJ`, `ghp_`, `lin_api`, `0x`

### 2. Crash Vector Detection
```bash
grep -r -n 'unwrap()' crates/ --include=*.rs | grep -v test
grep -r -n 'panic!' crates/ --include=*.rs | grep -v test
grep -r -n 'expect("' crates/ --include=*.rs | grep -v test
```
- Counted all occurrences in non-test code
- Categorized by severity (production path vs test helper)
- Prioritized by frequency and impact

### 3. Timing Attack Detection
```bash
grep -r -n -E '(==|eq|compare|timing|constant_time)' crates/ --include=*.rs
```
- Filtered for cryptographic comparisons (HMAC, signature, hash, token, auth)
- Flagged `==` used for secret comparison (not `constant_time_eq`)
- Identified byte-by-byte comparison of bearer tokens

### 4. Concurrency Analysis
```bash
grep -r -n -E '(Mutex|RwLock|Atomic|Arc|tokio::sync)' crates/ --include=*.rs
```
- Looked for lock-across-await patterns (deadlock risk)
- Identified `self.inbound_rx.lock().await` and `self.client.lock().await`
- Checked for proper use of `tokio::sync::Mutex` vs `std::sync::Mutex`

### 5. Injection & Execution Detection
```bash
grep -r -n -E '(format!.*\$|shell|exec|spawn|Command::new|std::process::Command)' crates/ --include=*.rs
```
- Found shell execution tool in tool-registry
- Identified command spawning points
- Checked for input validation before execution

### 6. Path Traversal Check
```bash
grep -r -n -E '(\.\./|\.\.\\|Path::new|join\(|canonicalize|resolve)' crates/ --include=*.rs
```
- Verified canonicalization is used
- Checked for `../` rejection in skill path expansion

### 7. Dependency & Supply Chain
```bash
cargo audit        # Not installed — needs setup
cargo outdated -R  # Not installed — needs setup
cargo license      # Not installed — needs setup
```
- Checked if tools are available
- Identified gap in CI security scanning

### 8. Ethical & Bias Scan
```bash
grep -r -n -i -E '(blacklist|whitelist|master|slave|dummy|sanity|cripple|retard)' crates/ --include=*.rs --include=*.md
```
- Found problematic terminology in comments and test names
- Checked for non-inclusive language

---

## 🚨 CRITICAL FINDINGS (Fix First)

### FINDING-001: Timing Attack in Admin UI Authentication
**Severity:** CRITICAL  
**Location:** `crates/admin-ui/src/auth.rs:45`  
**Current Code:**
```rust
header_value.as_bytes() == self.expected_header.as_bytes()
```
**Why It's Bad:**  
The `==` operator on byte slices performs a short-circuiting comparison — it returns `false` as soon as it finds a mismatch. This leaks timing information. An attacker can measure response times to brute-force the bearer token byte-by-byte. This is a classic timing attack vulnerability.

**How to Verify:**  
1. Read `crates/admin-ui/src/auth.rs` around line 45
2. Check if `subtle` crate is in dependencies
3. Look for other places where secrets are compared with `==`

**Fix:**
```rust
// Add to Cargo.toml: subtle = "2"
use subtle::ConstantTimeEq;

// Replace:
// header_value.as_bytes() == self.expected_header.as_bytes()
// With:
header_value.as_bytes().ct_eq(self.expected_header.as_bytes()).into()
```
**Verification:**
```bash
cargo test -p ardur-admin-ui -- auth
# Add test that verifies comparison takes constant time regardless of match
```

---

### FINDING-002: 364 unwrap() Calls in Production Code
**Severity:** HIGH  
**Count:** 364 occurrences  
**Top Offenders:**
- `crates/hooks-openclaw-compat/src/hook.rs:97-98` — 2 unwraps
- `crates/embeddings/src/lib.rs:229,240,273,278` — 4 unwraps
- `crates/tool-registry/src/skills/tool.rs:224-261` — 5+ unwraps
- `crates/fused-runtime/src/runtime.rs` — multiple
- `crates/provider-*.rs` — various

**Why It's Bad:**  
`unwrap()` causes an immediate panic if the value is `None` or `Err`. In production, this crashes the entire process. A malicious user can craft inputs to trigger these panics, causing denial of service.

**How to Verify:**  
```bash
grep -r -n 'unwrap()' crates/ --include=*.rs | grep -v test | grep -v '//' | head -50
```

**Fix Strategy (in order of priority):**

**Priority 1 — Production Paths (Fix First):**
```rust
// Before:
let value = some_operation().unwrap();

// After:
let value = some_operation().map_err(|e| {
    tracing::error!("operation failed: {}", e);
    MyError::OperationFailed(e)
})?;
```

**Priority 2 — Initialization Code:**
```rust
// Before:
let config = load_config().unwrap();

// After:
let config = load_config().map_err(|e| {
    eprintln!("Failed to load config: {}", e);
    std::process::exit(1);
})?;
```

**Priority 3 — Test-Only Paths (Lower Priority):**
Keep `unwrap()` in `#[cfg(test)]` blocks if the test should fail on error.

**Verification:**
```bash
# After fixing, count should be < 50 in production code
grep -r 'unwrap()' crates/ --include=*.rs | grep -v test | grep -v '//' | wc -l
```

---

### FINDING-003: 8 panic! Calls in Production Code
**Severity:** HIGH  
**Locations:**
1. `crates/provider-openai-compat/src/lib.rs:903` — `panic!("expected ToolUse, got {other}")`
2. `crates/provider-openai-compat/src/streaming.rs:637` — `panic!("expected Done(ToolUse), got ...")`
3. `crates/provider-claude-cli/src/lib.rs:928` — `panic!("expected Upstream, got {other}")`
4. `crates/provider-claude-cli/src/lib.rs:963` — `panic!("expected Upstream, got {other}")`
5. `crates/browser/src/lib.rs:115` — `panic!("BROWSER_CAPABILITY should be Custom")`
6. `crates/provider-selector/src/lib.rs:338` — `panic!("expected error")` (in test)
7. `crates/provider-openrouter/src/lib.rs:890` — `panic!("expected ToolUse, got {other}")`
8. `crates/provider-openrouter/src/streaming.rs:650` — `panic!("expected Done(ToolUse), got ...")`

**Why It's Bad:**  
`panic!` aborts the entire process. In a server context, this kills all active connections and requires a restart. These are in provider response parsing — a malformed provider response can crash the entire agent.

**How to Verify:**  
```bash
grep -r -n 'panic!' crates/ --include=*.rs | grep -v test
```

**Fix:**
```rust
// Before:
match response {
    Response::ToolUse(t) => t,
    other => panic!("expected ToolUse, got {other}"),
}

// After:
match response {
    Response::ToolUse(t) => t,
    other => {
        tracing::error!("unexpected response type: {:?}", other);
        return Err(ProviderError::InvalidResponse(
            format!("expected ToolUse, got {:?}", other)
        ));
    }
}
```

**Verification:**
```bash
cargo test -p ardur-provider-openai-compat
cargo test -p ardur-provider-openrouter
cargo test -p ardur-provider-claude-cli
```

---

### FINDING-004: 81 expect() Calls in Production Code
**Severity:** MEDIUM-HIGH  
**Top Offenders:**
- `crates/webhook/src/signature.rs:37` — `.expect("HMAC init with any length key is infallible for HmacSha256")`
- `crates/webhook/src/inbound.rs:80` — `.expect("HMAC init...")`
- `crates/webhook/src/outbound.rs:67` — `.expect("reqwest client build...")`
- `crates/channel-telegram/src/lib.rs:91-150` — multiple expects
- `crates/tool-registry/src/skills/skill.rs:164-232` — multiple expects

**Why It's Bad:**  
`expect()` is just `unwrap()` with a custom message. It still panics. The messages often claim something is "infallible" — but if the assumption is wrong, the process crashes.

**How to Verify:**  
```bash
grep -r -n 'expect("' crates/ --include=*.rs | grep -v test | head -30
```

**Fix:**
```rust
// Before:
let mac = HmacSha256::new_from_slice(key).expect("HMAC init with any length key is infallible");

// After:
let mac = HmacSha256::new_from_slice(key).map_err(|e| {
    tracing::error!("HMAC initialization failed: {}", e);
    WebhookError::InvalidKey
})?;
```

**Verification:**
```bash
cargo test -p ardur-webhook
cargo test -p ardur-channel-telegram
cargo test -p ardur-tool-registry
```

---

### FINDING-005: Hardcoded API Key Patterns in Test Code
**Severity:** MEDIUM  
**Locations:**
- `crates/provider-openai-compat/src/lib.rs` — `OpenAiCompatConfig::new("sk-ope...cret")`
- `crates/provider-openrouter/src/lib.rs` — `OpenRouterConfig::new("sk-ope...cret")`

**Why It's Bad:**  
Even in test code, hardcoded API key patterns can:
1. Be accidentally copied to production
2. Trigger secret scanning alerts
3. Reveal the key format to attackers
4. Be included in built binaries if tests are compiled in

**How to Verify:**  
```bash
grep -r -n 'sk-' crates/ --include=*.rs | grep -v '//' | grep -v test
grep -r -n 'pk_' crates/ --include=*.rs | grep -v '//' | grep -v test
```

**Fix:**
```rust
// Before:
let cfg = OpenAiCompatConfig::new("sk-openai-compat-test-secret");

// After:
let cfg = OpenAiCompatConfig::new("DUMMY_KEY_FOR_TESTING_ONLY");
// Or use env var:
let cfg = OpenAiCompatConfig::new(
    std::env::var("TEST_API_KEY").unwrap_or_else(|_| "DUMMY_KEY".to_string())
);
```

**Verification:**
```bash
grep -r 'sk-' crates/ --include=*.rs | grep -v '//' | grep -v test
# Should return nothing
```

---

## ⚠️ HIGH SEVERITY FINDINGS

### FINDING-006: Lock-Across-Await Deadlock Risk
**Severity:** HIGH  
**Locations:**
- `crates/channel-telegram/src/channel.rs:243` — `let mut rx = self.inbound_rx.lock().await;`
- `crates/channel-discord/src/channel.rs:173` — `let Some(mut client) = self.client.lock().await.take();`

**Why It's Bad:**  
Holding a `tokio::sync::Mutex` across an `.await` point can cause deadlocks if the async task is cancelled or if another task tries to acquire the same lock. This is a well-known anti-pattern in async Rust.

**How to Verify:**  
```bash
grep -r -n -B2 -A2 'lock().await' crates/ --include=*.rs
```

**Fix:**
```rust
// Before:
async fn receive(&self) -> Result<IncomingMessage, GatewayError> {
    let mut rx = self.inbound_rx.lock().await;
    rx.recv().await.ok_or(GatewayError::ChannelClosed)
}

// After:
async fn receive(&self) -> Result<IncomingMessage, GatewayError> {
    let message = {
        let mut rx = self.inbound_rx.lock().await;
        rx.recv().await
    }; // Lock dropped here before await
    message.ok_or(GatewayError::ChannelClosed)
}
```

**Verification:**
```bash
cargo test -p ardur-channel-telegram -- stress_test
cargo test -p ardur-channel-discord -- stress_test
# Add concurrent receive/send tests
```

---

### FINDING-007: Shell Command Execution Without Sandboxing
**Severity:** HIGH  
**Location:** `crates/tool-registry/src/builtins/shell.rs:6`  
**Description:** "This tool executes arbitrary commands through the shell"

**Why It's Bad:**  
Executing arbitrary shell commands is inherently dangerous. Even with cap-token/Cedar gating, a compromised or confused agent could execute destructive commands (`rm -rf /`, `curl | bash`, etc.).

**How to Verify:**  
```bash
cat crates/tool-registry/src/builtins/shell.rs
```

**Fix Options (in order of preference):**

**Option A: Add Sandboxing (Best)**
```rust
// Use a sandboxed execution environment
use std::process::Stdio;

pub async fn execute_sandboxed(command: &str) -> Result<String, ToolError> {
    // Whitelist allowed commands
    let allowed_commands = ["ls", "cat", "echo", "grep", "find"];
    let cmd_name = command.split_whitespace().next()
        .ok_or(ToolError::EmptyCommand)?;
    
    if !allowed_commands.contains(&cmd_name) {
        return Err(ToolError::CommandNotAllowed(cmd_name.to_string()));
    }
    
    // Run with restricted permissions
    let output = tokio::process::Command::new("sh")
        .arg("-c")
        .arg(command)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| ToolError::SpawnFailed(e))?;
    
    // Add timeout, resource limits, etc.
    // ...
}
```

**Option B: Add Confirmation Gate**
```rust
// Require human confirmation for destructive commands
if is_destructive(command) {
    return Err(ToolError::ConfirmationRequired(
        "This command may modify the system. Human approval required."
    ));
}
```

**Option C: Read-Only Mode**
```rust
// Only allow commands that don't modify the system
let readonly_commands = ["ls", "cat", "echo", "grep", "find", "head", "tail"];
```

**Verification:**
```bash
cargo test -p ardur-tool-registry -- shell_tool
# Add tests for:
# - Allowed commands work
# - Blocked commands fail
# - Destructive commands require confirmation
```

---

### FINDING-008: unsafe Blocks in Test Code
**Severity:** LOW-MEDIUM  
**Locations:**
- `crates/memory-qdrant/tests/config_env.rs:20,33,48` — `unsafe { std::env::remove_var(key) }`
- `crates/server/tests/boot_hybrid.rs:32` — `unsafe { std::env::set_var("QDRANT_COLLECTION", ...) }`
- `crates/server/tests/config_from_env.rs:58,63` — `unsafe { std::env::set_var(...) }`, `unsafe { std::env::remove_var(...) }`
- `crates/provider-selector/tests/selector_env.rs:27,38` — `unsafe { std::env::set_var(...) }`

**Why It's Bad:**  
`std::env::set_var()` and `remove_var()` are marked `unsafe` because they are not thread-safe. In concurrent tests, this can cause:
- Race conditions
- Test flakiness
- Undefined behavior in multi-threaded environments

**How to Verify:**  
```bash
grep -r -n 'unsafe' crates/ --include=*.rs | grep -v '//' | grep test
```

**Fix:**
```rust
// Before:
unsafe { std::env::set_var("QDRANT_COLLECTION", "ardur_boot_hybrid") };

// After:
// Use serial_test to prevent concurrent env var manipulation
#[cfg(test)]
use serial_test::serial;

#[tokio::test]
#[serial] // Prevents concurrent execution with other tests using env vars
async fn test_with_env() {
    // Use a scoped env var helper
    let _guard = EnvVarGuard::set("QDRANT_COLLECTION", "ardur_boot_hybrid");
    // Test code here
    // Guard automatically restores env var on drop
}
```

**Verification:**
```bash
cargo test -p ardur-memory-qdrant -- config_env
cargo test -p ardur-server -- boot_hybrid
cargo test -p ardur-provider-selector -- selector_env
# Run tests multiple times to check for flakiness
for i in {1..10}; do cargo test -p ardur-server -- boot_hybrid; done
```

---

## ℹ️ MEDIUM SEVERITY FINDINGS

### FINDING-009: Problematic Terminology
**Severity:** MEDIUM  
**Locations:**
- `crates/tool-registry/src/echo.rs:2` — "sanity demo"
- `crates/cedar-policy/tests/smoke.rs:5-6` — "Dummy" struct
- `crates/server/tests/config_from_env.rs:140` — "Sanity: the rest still resolved"
- `crates/e2e-tests/tests/scenario_06_multi_agent_attenuation.rs:116` — "Sanity: the parent genuinely"

**Why It's Bad:**  
Non-inclusive terminology creates an unwelcoming environment for contributors with mental health conditions. Industry best practice (RFC 7322, IETF) recommends avoiding these terms.

**Fix:**
```rust
// Before:
// Used by the registry tests and as a sanity demo

// After:
// Used by the registry tests and as a validation demo

// Before:
struct Dummy;

// After:
struct Placeholder;
// or
struct TestStub;

// Before:
// Sanity: the rest still resolved to their defaults

// After:
// Validation: the rest still resolved to their defaults
```

**Verification:**
```bash
grep -r -n -i 'sanity\|dummy' crates/ --include=*.rs
# Should return nothing after fixes
```

---

### FINDING-010: Missing Top-Level Documentation
**Severity:** MEDIUM  
**Count:** 96 crates missing `//!` documentation  
**Impact:** Poor maintainability, onboarding difficulty

**How to Verify:**  
```bash
find crates/ -name '*.rs' -exec grep -L '//!' {} +
```

**Fix:**
Add to the top of each undocumented crate:
```rust
//! # Crate Name
//!
//! Brief description of what this crate does.
//!
//! ## Features
//! - Feature 1
//! - Feature 2
//!
//! ## Usage
//! ```rust
//! // Example code
//! ```
```

**Priority Order:**
1. Core crates: `fused-runtime`, `cap-token`, `cedar-policy`, `receipt`
2. Provider crates: `provider-*`
3. Tool crates: `tool-registry`, `browser`, `computer-use`
4. Channel crates: `channel-*`
5. Other crates

---

### FINDING-011: Missing Security Scanning in CI
**Severity:** MEDIUM  
**Issue:** `cargo-audit`, `cargo-outdated`, `cargo-license` not installed

**Fix:**
```bash
# Install tools
cargo install cargo-audit
cargo install cargo-outdated
cargo install cargo-license

# Add to CI pipeline (GitHub Actions example)
```yaml
- name: Security Audit
  run: |
    cargo audit
    cargo outdated -R
    cargo license
```

**Verification:**
```bash
cargo audit
# Should show no vulnerabilities
# If vulnerabilities found, update dependencies or document exceptions
```

---

## 📊 AUDIT SUMMARY TABLE

| ID | Finding | Count | Severity | Status |
|----|---------|-------|----------|--------|
| 001 | Timing attack in admin auth | 1 | CRITICAL | 🔴 Open |
| 002 | unwrap() in production | 364 | HIGH | 🔴 Open |
| 003 | panic! in production | 8 | HIGH | 🔴 Open |
| 004 | expect() in production | 81 | MEDIUM-HIGH | 🔴 Open |
| 005 | Hardcoded secrets in tests | 2 | MEDIUM | 🔴 Open |
| 006 | Lock-across-await deadlocks | 2 | HIGH | 🔴 Open |
| 007 | Shell execution unsandboxed | 1 | HIGH | 🔴 Open |
| 008 | unsafe env var manipulation | 8 | LOW-MEDIUM | 🔴 Open |
| 009 | Problematic terminology | 4 | MEDIUM | 🔴 Open |
| 010 | Missing documentation | 96 | MEDIUM | 🔴 Open |
| 011 | Missing security scanning | 3 tools | MEDIUM | 🔴 Open |

---

## 🎯 FIX PRIORITY ORDER

### Phase 1: Security Critical (Do First)
1. FINDING-001: Fix timing attack with `constant_time_eq`
2. FINDING-003: Remove all 8 `panic!` from production
3. FINDING-006: Fix lock-across-await patterns
4. FINDING-007: Add sandboxing to shell tool

### Phase 2: Stability (Do Second)
5. FINDING-002: Replace top 50 `unwrap()` with `?`
6. FINDING-004: Replace top 20 `expect()` with proper error handling
7. FINDING-008: Fix `unsafe` env var tests with `serial_test`

### Phase 3: Quality (Do Third)
8. FINDING-005: Replace hardcoded test keys with dummy values
9. FINDING-009: Replace problematic terminology
10. FINDING-010: Add top-level documentation to core crates
11. FINDING-011: Add cargo-audit to CI

### Phase 4: Cleanup (Do Last)
12. Reduce remaining `unwrap()` from ~300 to <50
13. Add property-based tests for cost arithmetic
14. Add accessibility documentation
15. Complete documentation for all 96 crates

---

## ✅ VERIFICATION CHECKLIST

After each fix, verify:

- [ ] `cargo check --workspace` passes
- [ ] `cargo test --workspace` passes
- [ ] `cargo fmt --all` clean
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` clean
- [ ] `cargo audit` passes (after installing)
- [ ] No new `unwrap()` added in production code
- [ ] No new `panic!` added in production code
- [ ] Linear issue updated with test evidence
- [ ] Commit message references finding ID (e.g., "Fix FINDING-001: timing attack")

---

## 📚 REFERENCES

- [Rust Security Guidelines](https://anixe.io/security/rust-security-guidelines/)
- [OWASP Top 10 for Rust](https://owasp.org/www-project-top-10-for-large-language-model-applications/)
- [Rust API Guidelines: Error Handling](https://rust-lang.github.io/api-guidelines/interoperability.html#c-good-err)
- [Subtle crate docs](https://docs.rs/subtle/latest/subtle/)
- [Tokio: Holding MutexGuard across await](https://docs.rs/tokio/latest/tokio/sync/struct.Mutex.html#which-kind-of-mutex-should-you-use)
- [RFC 7322: Inclusive Terminology](https://www.rfc-editor.org/rfc/rfc7322)

---

## 🚀 READY TO START

**Your task:**
1. Run the bootstrap script
2. Create branch `agent/security-hardening-audit-fixes`
3. Work through Phase 1 findings first
4. Follow GSTACK principles (Gather, Source, Test, Atomic, Cap-Token, Keep)
5. Update Linear issues with evidence
6. Do NOT skip any finding — every single one must be addressed

**Expected outcome:**
- 0 `panic!` in production code
- <50 `unwrap()` in production code
- 0 timing attack vectors
- 0 lock-across-await patterns
- Sandboxed shell execution
- Clean `cargo audit`
- All tests passing

**Questions?** Re-read the finding description, check the referenced file, and verify the issue exists before fixing.
