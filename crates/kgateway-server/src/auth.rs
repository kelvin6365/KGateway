//! Security middleware: RBAC control-plane auth and a JSON nesting-depth guard.

use crate::app::SharedState;
use crate::config::{interpolate_env, ApiTokenConfig, Role};
use axum::body::{Body, Bytes};
use axum::extract::{Request, State};
use axum::http::{header, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use std::collections::HashMap;

/// Max JSON nesting depth accepted on request bodies. Beyond this, `serde_json`'s
/// recursive `Value` deserializer risks a stack overflow (which `abort()`s the whole
/// process, not just one task), so we reject deep bodies before they reach serde.
const MAX_JSON_DEPTH: usize = 64;

/// Max request body we buffer for the depth check (matches axum's default body limit).
const MAX_BODY_BYTES: usize = 2 * 1024 * 1024;

/// A control-plane permission. Roles map to sets of these (see [`Role::permits`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Permission {
    /// Read logs / metrics / config (all roles).
    LogsView,
    /// Mutate config — providers, virtual keys (operator, admin).
    ConfigWrite,
    /// Reveal redacted log content (admin only).
    LogsReveal,
}

impl Role {
    /// Whether this role is granted `perm`. Roles are hierarchical:
    /// viewer ⊂ operator ⊂ admin.
    pub fn permits(self, perm: Permission) -> bool {
        match perm {
            Permission::LogsView => true,
            Permission::ConfigWrite => matches!(self, Role::Operator | Role::Admin),
            Permission::LogsReveal => matches!(self, Role::Admin),
        }
    }
}

/// A token's resolved identity: its role and a display name (for audit).
#[derive(Clone)]
struct Identity {
    role: Role,
    name: String,
}

/// Resolved token → identity table for the control plane. Built once at startup from the
/// legacy `admin_token` plus `api_tokens`.
///
/// **Fail-closed**: if tokens were *declared* in config but ALL of them resolved to empty
/// (e.g. every `${ENV}` reference is missing), auth is still ENFORCED and every request is
/// rejected (401) — a broken secret injection must not silently open the control plane.
/// Only a config with no tokens declared at all runs in open (dev) mode.
pub struct AuthContext {
    tokens: HashMap<String, Identity>,
    /// Whether any token was declared in config (regardless of whether it resolved).
    declared: bool,
}

impl AuthContext {
    /// Build from the legacy admin token (→ admin role) and the `api_tokens` list. `${ENV}`
    /// references are interpolated; empty (unresolved) tokens are dropped from the table but
    /// still count as "declared" for the fail-closed guard.
    pub fn from_config(admin_token: Option<&str>, api_tokens: &[ApiTokenConfig]) -> Self {
        let declared = admin_token.is_some() || !api_tokens.is_empty();
        let mut tokens = HashMap::new();
        for t in api_tokens {
            let resolved = interpolate_env(&t.token);
            if !resolved.is_empty() {
                let name = if t.name.is_empty() {
                    format!("{:?}", t.role).to_lowercase()
                } else {
                    t.name.clone()
                };
                tokens.insert(resolved, Identity { role: t.role, name });
            }
        }
        if let Some(a) = admin_token {
            let resolved = interpolate_env(a);
            if !resolved.is_empty() {
                tokens.insert(
                    resolved,
                    Identity {
                        role: Role::Admin,
                        name: "admin_token".to_string(),
                    },
                );
            }
        }
        Self { tokens, declared }
    }

    /// Auth is enforced when at least one token resolved OR tokens were declared but all
    /// resolved empty (fail-closed). Only a fully token-less config runs open (dev).
    pub fn is_enabled(&self) -> bool {
        !self.tokens.is_empty() || self.declared
    }

    /// True when tokens were declared but none resolved — the control plane is locked and
    /// no request can authenticate. Surfaced as a loud startup error.
    pub fn is_locked(&self) -> bool {
        self.declared && self.tokens.is_empty()
    }

    /// Resolve a presented bearer token to its role, if known.
    pub fn role_for(&self, token: &str) -> Option<Role> {
        self.identify(token).map(|(role, _)| role)
    }

    /// Resolve a presented bearer token to its (role, name) identity, if known. The full
    /// map is scanned without an early `break`, but note the per-entry byte compare
    /// (`[u8]::eq`) is NOT constant-time — over HTTP the timing signal is negligible; use a
    /// constant-time compare here if that ever becomes a real requirement.
    pub fn identify(&self, token: &str) -> Option<(Role, String)> {
        let mut found = None;
        for (k, id) in &self.tokens {
            if k.len() == token.len() && k.as_bytes() == token.as_bytes() {
                found = Some((id.role, id.name.clone()));
            }
        }
        found
    }
}

/// Extract a `Bearer` token from the `Authorization` header.
pub fn bearer_token(req: &Request) -> Option<&str> {
    req.headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::trim)
}

fn unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        axum::Json(serde_json::json!({
            "error": { "message": "authentication required", "type": "auth" }
        })),
    )
        .into_response()
}

fn forbidden() -> Response {
    (
        StatusCode::FORBIDDEN,
        axum::Json(serde_json::json!({
            "error": { "message": "insufficient permission", "type": "forbidden" }
        })),
    )
        .into_response()
}

/// Core check: pass through when auth is disabled, else require a known token whose role
/// grants `perm` (401 unknown/absent, 403 insufficient).
async fn require(state: &SharedState, req: Request, next: Next, perm: Permission) -> Response {
    if !state.auth.is_enabled() {
        return next.run(req).await; // auth disabled (dev)
    }
    match bearer_token(&req).and_then(|t| state.auth.role_for(t)) {
        Some(role) if role.permits(perm) => next.run(req).await,
        Some(_) => forbidden(),
        None => unauthorized(),
    }
}

/// Middleware: require `logs:view` (any authenticated role).
pub async fn require_view(State(state): State<SharedState>, req: Request, next: Next) -> Response {
    require(&state, req, next, Permission::LogsView).await
}

/// Middleware: require `config:write` (operator / admin).
pub async fn require_write(State(state): State<SharedState>, req: Request, next: Next) -> Response {
    require(&state, req, next, Permission::ConfigWrite).await
}

/// Middleware: require `logs:reveal` (admin).
pub async fn require_reveal(
    State(state): State<SharedState>,
    req: Request,
    next: Next,
) -> Response {
    require(&state, req, next, Permission::LogsReveal).await
}

/// Buffer the request body and reject it if its JSON nesting depth exceeds
/// `MAX_JSON_DEPTH`, before any handler/serde deserialization runs.
pub async fn json_depth_guard(req: Request, next: Next) -> Response {
    // Only bodies that look like JSON are worth scanning.
    let is_json = req
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.contains("application/json"));
    if !is_json {
        return next.run(req).await;
    }

    let (parts, body) = req.into_parts();
    let bytes = match axum::body::to_bytes(body, MAX_BODY_BYTES).await {
        Ok(b) => b,
        Err(_) => return (StatusCode::PAYLOAD_TOO_LARGE, "request body too large").into_response(),
    };

    if json_depth(&bytes) > MAX_JSON_DEPTH {
        return (
            StatusCode::BAD_REQUEST,
            axum::Json(serde_json::json!({
                "error": { "message": "request JSON nested too deeply", "type": "bad_request" }
            })),
        )
            .into_response();
    }

    // Rebuild the request with the buffered body for downstream handlers.
    let req = Request::from_parts(parts, Body::from(bytes));
    next.run(req).await
}

/// Maximum bracket-nesting depth of a JSON byte buffer. String contents (and escaped
/// quotes/brackets within them) are skipped so `"[[[["` in a string doesn't count.
fn json_depth(bytes: &Bytes) -> usize {
    let mut depth = 0usize;
    let mut max = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for &b in bytes.iter() {
        if in_string {
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == b'"' {
                in_string = false;
            }
            continue;
        }
        match b {
            b'"' => in_string = true,
            b'{' | b'[' => {
                depth += 1;
                max = max.max(depth);
            }
            b'}' | b']' => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    max
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tok(token: &str, role: Role) -> ApiTokenConfig {
        ApiTokenConfig {
            token: token.into(),
            role,
            name: String::new(),
        }
    }

    #[test]
    fn permission_matrix() {
        use Permission::*;
        // viewer: view only
        assert!(Role::Viewer.permits(LogsView));
        assert!(!Role::Viewer.permits(ConfigWrite));
        assert!(!Role::Viewer.permits(LogsReveal));
        // operator: view + write, not reveal
        assert!(Role::Operator.permits(LogsView));
        assert!(Role::Operator.permits(ConfigWrite));
        assert!(!Role::Operator.permits(LogsReveal));
        // admin: everything
        assert!(Role::Admin.permits(LogsView));
        assert!(Role::Admin.permits(ConfigWrite));
        assert!(Role::Admin.permits(LogsReveal));
    }

    #[test]
    fn resolves_tokens_to_roles() {
        let ctx = AuthContext::from_config(
            None,
            &[tok("view-tok", Role::Viewer), tok("op-tok", Role::Operator)],
        );
        assert!(ctx.is_enabled());
        assert_eq!(ctx.role_for("view-tok"), Some(Role::Viewer));
        assert_eq!(ctx.role_for("op-tok"), Some(Role::Operator));
        assert_eq!(ctx.role_for("unknown"), None);
    }

    #[test]
    fn legacy_admin_token_maps_to_admin() {
        // Backward compat: a bare admin_token still authorizes as admin.
        let ctx = AuthContext::from_config(Some("legacy"), &[]);
        assert!(ctx.is_enabled());
        assert_eq!(ctx.role_for("legacy"), Some(Role::Admin));
        assert!(ctx
            .role_for("legacy")
            .unwrap()
            .permits(Permission::LogsReveal));
    }

    #[test]
    fn no_tokens_means_auth_disabled() {
        let ctx = AuthContext::from_config(None, &[]);
        assert!(!ctx.is_enabled());
        assert!(!ctx.is_locked());
    }

    #[test]
    fn declared_but_all_empty_is_locked_not_open() {
        // A token declared via ${ENV} that resolves empty (env unset) must NOT open the
        // control plane — it locks it (fail-closed): enforced, but no token authenticates.
        let ctx = AuthContext::from_config(
            None,
            &[tok("${KGATEWAY_DEFINITELY_MISSING_ENV_XYZ}", Role::Admin)],
        );
        assert!(ctx.is_locked());
        assert!(ctx.is_enabled(), "locked means still enforced (deny all)");
        assert_eq!(ctx.role_for("anything"), None);
    }

    #[test]
    fn identify_returns_name_for_audit() {
        let ctx = AuthContext::from_config(
            None,
            &[ApiTokenConfig {
                token: "t".into(),
                role: Role::Admin,
                name: "alice".into(),
            }],
        );
        assert_eq!(ctx.identify("t"), Some((Role::Admin, "alice".to_string())));
        // Legacy admin_token gets a stable audit name.
        let legacy = AuthContext::from_config(Some("x"), &[]);
        assert_eq!(legacy.identify("x").unwrap().1, "admin_token");
    }

    #[test]
    fn depth_counts_nesting_and_skips_strings() {
        assert_eq!(json_depth(&Bytes::from_static(b"{}")), 1);
        assert_eq!(json_depth(&Bytes::from_static(b"{\"a\":[1,[2,[3]]]}")), 4);
        // brackets inside a string do not count
        assert_eq!(json_depth(&Bytes::from_static(b"{\"a\":\"[[[[[[\"}")), 1);
        // escaped quote inside a string
        assert_eq!(json_depth(&Bytes::from_static(b"{\"a\":\"x\\\"[[\"}")), 1);
    }

    #[test]
    fn deep_nesting_exceeds_limit() {
        let deep = "[".repeat(MAX_JSON_DEPTH + 5);
        assert!(json_depth(&Bytes::from(deep)) > MAX_JSON_DEPTH);
    }
}
