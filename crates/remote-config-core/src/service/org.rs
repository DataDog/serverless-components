//! Organisation status and credential management helpers.
//!
//! This module centralises the logic for polling organisation status endpoints,
//! maintaining the cached organisation UUID, and handling credential rotation.
//! It mirrors the Go agent behaviour so that the surrounding refresh loop can
//! remain focused on periodic scheduling.

use std::sync::Arc;

use tokio::sync::broadcast;
use tokio::time::sleep;
use tracing::{debug, error, info, warn};

use super::config::MIN_REFRESH_INTERVAL;
use super::refresh::ServiceShared;
use super::ServiceError;
use crate::http::HttpError;
use crate::rc_key::RcKey;
use crate::store::StoreError;
use crate::uptane::UptaneError;

impl ServiceShared {
    /// Runs the organisation status poller loop until a shutdown signal is observed.
    ///
    /// Implements the same exponential backoff/escalation behaviour as the Go
    /// agent: repeated 503/504 errors escalate to error logs while other
    /// failures remain warnings.
    pub(crate) async fn run_org_status_poller(
        self: Arc<Self>,
        shutdown_rx: &mut broadcast::Receiver<()>,
    ) {
        if self.config.org_status_interval.is_zero() {
            return;
        }
        let mut delay = self.config.org_status_interval;
        let mut retry_delay = MIN_REFRESH_INTERVAL.min(self.config.org_status_interval);
        let mut immediate = true;
        loop {
            if immediate {
                if shutdown_rx.try_recv().is_ok() {
                    debug!("remote-config org status poller shutting down");
                    break;
                }
            } else {
                tokio::select! {
                    _ = shutdown_rx.recv() => {
                        debug!("remote-config org status poller shutting down");
                        break;
                    }
                    _ = sleep(delay) => {}
                }
                // Check for shutdown before starting HTTP request
                if shutdown_rx.try_recv().is_ok() {
                    debug!("remote-config org status poller shutting down");
                    break;
                }
            }
            immediate = false;
            if let Err(err) = self.poll_org_status().await {
                let escalate = matches!(err, ServiceError::Http(HttpError::Retryable(code)) if matches!(code, 503 | 504))
                    && {
                        let guard = self.state.lock().await;
                        guard.org_status_fetch_503_count
                            >= super::state::MAX_ORG_STATUS_FETCH_503_LOG_LEVEL
                    };
                if escalate {
                    error!(
                        "remote-config status poll failed: {err}; retrying in {:?}",
                        retry_delay
                    );
                } else {
                    warn!(
                        "remote-config status poll failed: {err}; retrying in {:?}",
                        retry_delay
                    );
                }
                delay = retry_delay;
                retry_delay = (retry_delay * 2).min(self.config.org_status_interval);
                retry_delay = retry_delay.max(MIN_REFRESH_INTERVAL);
                continue;
            }
            {
                let mut guard = self.state.lock().await;
                guard.reset_org_status_fetch_errors();
            }
            delay = self.config.org_status_interval;
            retry_delay = MIN_REFRESH_INTERVAL.min(self.config.org_status_interval);
        }
    }

    /// Ensures the cached organisation UUID is populated, refreshing it when missing.
    pub(crate) async fn ensure_org_uuid(&self) -> Result<(), ServiceError> {
        // Branch 1: in-memory state is already initialised, so skip any IO work.
        let has_org_uuid = {
            let guard = self.state.lock().await;
            guard.org_uuid.is_some()
        };
        if has_org_uuid {
            return Ok(());
        }

        // Branch 2: sled-backed cache already persisted the UUID – hydrate the mutex and return.
        if let Some(uuid) = self.uptane.stored_org_uuid()? {
            let mut guard = self.state.lock().await;
            guard.org_uuid = Some(uuid);
            return Ok(());
        }

        // Branch 3: nothing cached; fetch from the backend and persist it before updating state.
        let response = self.http_client.fetch_org_data().await?;
        let uuid = response.uuid;
        if !uuid.is_empty() {
            self.uptane.update_org_uuid(&uuid)?;
        }
        let mut guard = self.state.lock().await;
        guard.org_uuid = if uuid.is_empty() { None } else { Some(uuid) };
        Ok(())
    }

    /// Validates that the config snapshot embeds the same org UUID stored locally.
    pub(crate) async fn verify_snapshot_org_uuid(&self) -> Result<(), ServiceError> {
        // Skip verification entirely when the snapshot does not advertise an org binding.
        let Some(snapshot_uuid) = self.uptane.snapshot_org_uuid()? else {
            return Ok(());
        };
        // Ensure we have a stored org UUID to compare against (fetching if the cache is empty).
        self.ensure_org_uuid().await?;
        let stored_uuid = {
            let guard = self.state.lock().await;
            guard.org_uuid.clone()
        };
        let Some(stored_uuid) = stored_uuid else {
            // The backend has not provided an org UUID yet, so we cannot enforce a comparison.
            return Ok(());
        };
        if stored_uuid != snapshot_uuid {
            // Diverging UUIDs indicate that we might be serving metadata from a different organisation.
            return Err(ServiceError::Uptane(UptaneError::OrgUuidMismatch {
                stored: stored_uuid,
                snapshot: snapshot_uuid,
            }));
        }
        Ok(())
    }

    /// Fetches the latest organisation status flags and updates the cached snapshot.
    pub(crate) async fn poll_org_status(&self) -> Result<(), ServiceError> {
        // Poll the backend; we log and escalate differently depending on whether the request
        // succeeded or hit a retryable 5xx.
        let status = match self.http_client.fetch_org_status().await {
            Ok(status) => {
                // Success path: reset the fetch error counters so log levels drop back to normal.
                let mut guard = self.state.lock().await;
                guard.reset_org_status_fetch_errors();
                status
            }
            Err(err) => {
                {
                    let mut guard = self.state.lock().await;
                    if matches!(err, HttpError::Retryable(code) if matches!(code, 503 | 504)) {
                        // Repeated 503/504 responses escalate future log lines from warn→error.
                        guard.record_org_status_fetch_error();
                    } else {
                        guard.reset_org_status_fetch_errors();
                    }
                }
                return Err(ServiceError::Http(err));
            }
        };
        let change = {
            let mut guard = self.state.lock().await;
            guard.update_org_status(status)
        };
        if let Some((previous, current)) = change {
            // Org status toggled; emit the Go-compatible log message plus telemetry notification.
            let message = match (current.enabled, current.authorized) {
                (true, true) => "Remote Configuration is enabled for this organization and agent.",
                (true, false) => "Remote Configuration is enabled for this organization but disabled for this agent. Add the Remote Configuration Read permission to its API key to enable it for this agent.",
                (false, true) => "Remote Configuration is disabled for this organization.",
                (false, false) => "Remote Configuration is disabled for this organization and agent.",
            };
            info!("remote-config: {message}");
            let telemetry = self.telemetry.read().await.clone();
            telemetry.on_org_status_change(previous, current);
            self.status
                .set_org_flags(current.enabled, current.authorized);
        }
        Ok(())
    }

    /// Replaces authentication material and verifies that the organisation remains consistent.
    pub(crate) async fn rotate_credentials(
        &self,
        api_key: &str,
        rc_key: Option<&str>,
        par_jwt: Option<&str>,
    ) -> Result<(), ServiceError> {
        let result = self
            .rotate_credentials_inner(api_key, rc_key, par_jwt)
            .await;
        match &result {
            Ok(_) => {
                self.status.set_last_error(None).await;
                let org_uuid = {
                    let guard = self.state.lock().await;
                    guard.org_uuid.clone()
                };
                info!(
                    org_uuid = org_uuid.as_deref().unwrap_or("unknown"),
                    agent_version = %self.config.agent_version,
                    "remote-config credentials rotated"
                );
            }
            Err(err) => self.status.set_last_error(Some(err.to_string())).await,
        }
        result
    }

    /// Performs the credential rotation after status bookkeeping has been updated.
    async fn rotate_credentials_inner(
        &self,
        api_key: &str,
        rc_key: Option<&str>,
        par_jwt: Option<&str>,
    ) -> Result<(), ServiceError> {
        self.http_client.update_api_key(api_key).await?;
        let decoded_rc_key = match rc_key {
            // RC key provided: decode it before updating HTTP headers so application key rotation
            // mirrors the Go agent.
            Some(value) => {
                let decoded = RcKey::decode(value).map_err(|err| {
                    ServiceError::InvalidRequest(format!("invalid rc key: {err}"))
                })?;
                Some(decoded)
            }
            // No RC key: leave the application key header unset/cleared.
            None => None,
        };
        self.http_client
            .update_application_key(
                decoded_rc_key
                    .as_ref()
                    .map(|decoded| decoded.app_key.as_str()),
            )
            .await?;
        self.http_client.update_par_jwt(par_jwt).await?;

        self.uptane.update_metadata(
            &self.config.agent_version,
            api_key,
            self.http_client.base_url(),
        )?;

        let stored_uuid = self.uptane.stored_org_uuid()?;
        match self.http_client.fetch_org_data().await {
            // Success path: compare the newly fetched UUID with whatever we previously stored and
            // update sled + in-memory state when appropriate.
            Ok(response) => {
                let new_uuid = response.uuid;
                if let Some(stored) = stored_uuid.as_deref() {
                    if !new_uuid.is_empty() && stored != new_uuid {
                        // Cross-org credential swap detected; keep logging loudly for operators.
                        error!(
                            stored_uuid = stored,
                            updated_uuid = new_uuid,
                            "remote-config detected credential switch across organisations"
                        );
                    } else if !new_uuid.is_empty() {
                        // Same org; refresh sled + in-memory state with the latest UUID.
                        self.uptane.update_org_uuid(&new_uuid)?;
                        let mut guard = self.state.lock().await;
                        guard.org_uuid = Some(new_uuid);
                    }
                } else if !new_uuid.is_empty() {
                    // First-time bootstrap where no UUID existed previously.
                    self.uptane.update_org_uuid(&new_uuid)?;
                    let mut guard = self.state.lock().await;
                    guard.org_uuid = Some(new_uuid);
                }
            }
            // Failure: we keep the previous UUID if available and just log the inability to refresh.
            Err(err) => {
                warn!("remote-config failed to refresh org uuid after credential rotation: {err}");
            }
        }

        if stored_uuid.is_some() {
            let mut guard = self.state.lock().await;
            if guard.org_uuid.is_none() {
                // Safeguard: if the new fetch failed to set anything, fall back to the stored UUID.
                guard.org_uuid = stored_uuid;
            }
        }
        let expected_org = decoded_rc_key.as_ref().map(|key| key.org_id);
        self.uptane.set_expected_org_id(expected_org);
        self.update_rc_key(decoded_rc_key);
        Ok(())
    }

    /// Retrieves the backend client state blob echoed in refresh requests.
    ///
    /// This is the same opaque value the Go agent includes in
    /// `LatestConfigsRequest.backend_client_state`.
    pub(crate) fn backend_client_state(&self) -> Result<Vec<u8>, ServiceError> {
        let targets = match self.uptane.director_targets() {
            Ok(value) => value,
            Err(UptaneError::Store(StoreError::MissingMetadata)) => return Ok(Vec::new()),
            Err(err) => return Err(ServiceError::Uptane(err)),
        };
        super::client::extract_backend_client_state(targets)
    }
}

#[cfg(test)]
mod tests {
    use crate::service::test_support::{base_config, build_service_with_config};
    use httptest::Server;

    /// Ensures `ensure_org_uuid` hydrates the in-memory state from the persisted store.
    #[tokio::test]
    async fn ensure_org_uuid_populates_state_from_store() {
        let server = Server::run();
        let service = build_service_with_config(&server, base_config());
        let shared = service.shared_for_tests().clone();
        {
            let mut guard = shared.state.lock().await;
            guard.org_uuid = None;
        }
        shared.ensure_org_uuid().await.expect("org uuid loads");
        let guard = shared.state.lock().await;
        assert_eq!(
            guard.org_uuid.as_deref(),
            Some("00000000-0000-0000-0000-000000000000")
        );
    }

    /// Verifies rotating credentials rejects malformed RC keys before performing IO.
    #[tokio::test]
    async fn rotate_credentials_rejects_invalid_rc_key() {
        let server = Server::run();
        let service = build_service_with_config(&server, base_config());
        let shared = service.shared_for_tests().clone();
        let err = shared
            .rotate_credentials("api-key", Some("not-a-valid-key"), None)
            .await
            .expect_err("invalid rc key");
        assert!(
            err.to_string().contains("invalid rc key"),
            "unexpected error: {err}"
        );
    }
}
