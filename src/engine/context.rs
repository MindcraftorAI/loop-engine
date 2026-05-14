//! Per-request identity bundle.
//!
//! Threaded through every engine public function as `ctx: &Context`.
//! Cheap to `Clone` (24 bytes + four `Arc<str>`s). Never global, never
//! task-local — explicit parameter passing only (object_store / sqlx
//! pattern, not tracing's task-local pattern).
//!
//! `#[non_exhaustive]` so we can add `agent_id`, `request_id`, etc.
//! later without a breaking change. Note: external code constructs
//! `Context` only through [`Context::single_user_local`] or
//! [`Context::builder`] — never field-init literal.

use std::sync::Arc;

/// Per-request identity bundle.
///
/// See module docs for ownership conventions. Lifetime is per-request
/// (or per-session in single-user mode); cheap to `Clone` if needed
/// to move into a spawned task, but the engine prefers `&Context`.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct Context {
    pub tenant_id: TenantId,
    pub user_id: UserId,
    pub session_id: SessionId,
    pub team_id: Option<TeamId>,
}

/// Tenant identifier. Opaque to the engine; the host provides the value.
/// Single-user mode uses the literal `"local"`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TenantId(Arc<str>);

/// End-user identifier. In single-user mode, this is the OS username
/// (best effort) or `"default"` if `whoami` fails.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct UserId(Arc<str>);

/// Session identifier — one logical conversation, agent run, or workflow.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SessionId(Arc<str>);

/// Team identifier — present only when the user belongs to a team-scoped
/// workspace (MASA-style team learning). `None` for individual usage.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TeamId(Arc<str>);

macro_rules! impl_id_newtype {
    ($name:ident) => {
        impl $name {
            pub fn new(s: impl Into<Arc<str>>) -> Self {
                Self(s.into())
            }
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }
        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                &self.0
            }
        }
        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(&self.0)
            }
        }
    };
}

impl_id_newtype!(TenantId);
impl_id_newtype!(UserId);
impl_id_newtype!(SessionId);
impl_id_newtype!(TeamId);

impl Context {
    /// Single-user local default. Today's behavior:
    ///
    /// - `tenant_id` = `"local"`
    /// - `user_id`   = OS username (best effort) or `"default"`
    /// - `session_id` = a UUID generated at startup
    /// - `team_id`   = `None`
    ///
    /// Constructed once per daemon startup in single-user mode. The
    /// single-user "tenant=local" sentinel triggers `StorageKey`
    /// constructors to elide the `tenants/{id}/users/{id}/` prefix —
    /// today's on-disk layout is preserved unchanged.
    pub fn single_user_local() -> Self {
        Self {
            tenant_id: TenantId::new("local"),
            user_id: UserId::new(default_user_id()),
            session_id: SessionId::new(generate_session_id()),
            team_id: None,
        }
    }

    /// Builder for multi-tenant / SaaS / team-scoped contexts.
    pub fn builder() -> ContextBuilder {
        ContextBuilder::default()
    }
}

/// Builder for [`Context`]. Required fields panic at `build()` if unset
/// — but every well-formed caller knows what they have. Use
/// [`Context::single_user_local`] for the local default.
#[derive(Default)]
pub struct ContextBuilder {
    tenant_id: Option<TenantId>,
    user_id: Option<UserId>,
    session_id: Option<SessionId>,
    team_id: Option<TeamId>,
}

impl ContextBuilder {
    pub fn tenant_id(mut self, id: impl Into<Arc<str>>) -> Self {
        self.tenant_id = Some(TenantId::new(id));
        self
    }
    pub fn user_id(mut self, id: impl Into<Arc<str>>) -> Self {
        self.user_id = Some(UserId::new(id));
        self
    }
    pub fn session_id(mut self, id: impl Into<Arc<str>>) -> Self {
        self.session_id = Some(SessionId::new(id));
        self
    }
    pub fn team_id(mut self, id: impl Into<Arc<str>>) -> Self {
        self.team_id = Some(TeamId::new(id));
        self
    }
    pub fn build(self) -> Context {
        Context {
            tenant_id: self
                .tenant_id
                .expect("ContextBuilder: tenant_id is required"),
            user_id: self.user_id.expect("ContextBuilder: user_id is required"),
            session_id: self
                .session_id
                .expect("ContextBuilder: session_id is required"),
            team_id: self.team_id,
        }
    }
}

fn default_user_id() -> String {
    std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_else(|_| "default".to_string())
}

fn generate_session_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("session-{now:x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_user_local_sets_local_tenant() {
        let ctx = Context::single_user_local();
        assert_eq!(ctx.tenant_id.as_str(), "local");
        assert!(ctx.team_id.is_none());
        assert!(!ctx.user_id.as_str().is_empty());
        assert!(ctx.session_id.as_str().starts_with("session-"));
    }

    #[test]
    fn builder_constructs_multi_tenant_context() {
        let ctx = Context::builder()
            .tenant_id("acme-corp")
            .user_id("alice@acme")
            .session_id("sess-42")
            .team_id("auth-platform")
            .build();
        assert_eq!(ctx.tenant_id.as_str(), "acme-corp");
        assert_eq!(ctx.user_id.as_str(), "alice@acme");
        assert_eq!(ctx.session_id.as_str(), "sess-42");
        assert_eq!(ctx.team_id.unwrap().as_str(), "auth-platform");
    }

    #[test]
    fn ids_are_cheap_to_clone() {
        // Arc<str> ⇒ clone is atomic refcount bump, not a string copy.
        let id = TenantId::new("acme");
        let cloned = id.clone();
        assert_eq!(id.as_str(), cloned.as_str());
        // No way to assert "no allocation" from a unit test directly, but
        // the type system guarantees it (Arc<str> backing).
    }

    #[test]
    #[should_panic(expected = "tenant_id is required")]
    fn builder_panics_without_tenant() {
        let _ = Context::builder().user_id("u").session_id("s").build();
    }
}
