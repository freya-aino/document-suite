//! DAG-list provider for the `page` role: resolves a task to a validated `Router`.
//!
//! Routers are cached per DAG-list version (a version is immutable once uploaded).
//! The tenant → active-version mapping is cached for `refresh` and re-read from
//! Postgres on the next access after that; no background task. A pinned version
//! (`JobSpec::dag_list_version`) bypasses the active mapping.
//!
//! No active list at all is a *transient* error: the tenant's tasks wait (with the bus
//! backoff) while an operator activates a list, instead of being dead-lettered.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::RwLock;

use vs_core::TenantId;
use vs_dag::StepRegistry;
use vs_router::{DagList, Router};
use vs_vault::ops::Ops;

use crate::runtime::TaskError;

pub struct DagLists {
    ops: Arc<Ops>,
    steps: Arc<StepRegistry>,
    routers: RwLock<HashMap<String, Arc<Router>>>,
    active: RwLock<ActiveCache>,
}

impl DagLists {
    pub fn new(ops: Arc<Ops>, steps: Arc<StepRegistry>, refresh: Duration) -> Self {
        Self { ops, steps, routers: RwLock::new(HashMap::new()), active: RwLock::new(ActiveCache::new(refresh)) }
    }

    /// Router for `pinned`, else for the tenant's active list (tenant → global).
    pub async fn router_for(&self, tenant: &TenantId, pinned: Option<&str>) -> Result<Arc<Router>, TaskError> {
        if let Some(version) = pinned {
            if let Some(r) = self.routers.read().await.get(version) {
                return Ok(r.clone());
            }
            let list = self
                .ops
                .dag_list_get(version)
                .await?
                .ok_or_else(|| TaskError::permanent(anyhow::anyhow!("pinned DAG list {version:?} does not exist")))?;
            return self.load(list).await;
        }
        let now = Instant::now();
        let cached = self.active.read().await.get(tenant.as_str(), now).map(str::to_owned);
        let version = match cached {
            Some(v) => v,
            None => {
                let (version, list) = self
                    .ops
                    .active_dag_list(tenant)
                    .await?
                    .ok_or_else(|| anyhow::anyhow!("no active DAG list for tenant {} (activate one via vs-api)", tenant.as_str()))?;
                if self.routers.read().await.get(&version).is_none() {
                    self.load(list).await?;
                }
                self.active.write().await.put(tenant.as_str(), &version, now);
                version
            }
        };
        self.routers.read().await.get(&version).cloned().ok_or_else(|| TaskError::from(anyhow::anyhow!("router {version} vanished from cache")))
    }

    /// Validates against this worker's step registry and caches. A list that fails
    /// validation here was accepted by another binary version; retrying lets an
    /// operator activate a compatible list.
    async fn load(&self, list: DagList) -> Result<Arc<Router>, TaskError> {
        let version = list.version.clone();
        let router =
            Arc::new(Router::load(list, self.steps.as_ref()).map_err(|e| anyhow::anyhow!("DAG list {version} is not valid for this worker: {e}"))?);
        self.routers.write().await.entry(version).or_insert(router.clone());
        Ok(router)
    }
}

/// Pure part of the active mapping: tenant → (version, fetched-at), expiring after
/// `ttl`. Takes `now` as an argument so it can be tested with a fake clock.
#[derive(Debug)]
pub struct ActiveCache {
    ttl: Duration,
    entries: HashMap<String, (String, Instant)>,
}

impl ActiveCache {
    pub fn new(ttl: Duration) -> Self {
        Self { ttl, entries: HashMap::new() }
    }

    pub fn get(&self, tenant: &str, now: Instant) -> Option<&str> {
        let (version, at) = self.entries.get(tenant)?;
        (now.saturating_duration_since(*at) < self.ttl).then_some(version.as_str())
    }

    pub fn put(&mut self, tenant: &str, version: &str, now: Instant) {
        self.entries.insert(tenant.to_string(), (version.to_string(), now));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn active_mapping_expires_after_ttl() {
        let t0 = Instant::now();
        let mut c = ActiveCache::new(Duration::from_secs(60));
        assert_eq!(c.get("acme", t0), None);
        c.put("acme", "v1", t0);
        assert_eq!(c.get("acme", t0), Some("v1"));
        assert_eq!(c.get("acme", t0 + Duration::from_secs(59)), Some("v1"));
        assert_eq!(c.get("acme", t0 + Duration::from_secs(60)), None, "stale after ttl");
        assert_eq!(c.get("other", t0), None, "per tenant");
        c.put("acme", "v2", t0 + Duration::from_secs(60));
        assert_eq!(c.get("acme", t0 + Duration::from_secs(61)), Some("v2"));
    }

    #[test]
    fn zero_ttl_never_caches() {
        let t0 = Instant::now();
        let mut c = ActiveCache::new(Duration::ZERO);
        c.put("acme", "v1", t0);
        assert_eq!(c.get("acme", t0), None);
    }
}
