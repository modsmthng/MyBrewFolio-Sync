// SPDX-License-Identifier: GPL-3.0-or-later

use super::{
    log_http_failure, CloudClient, CloudError, DeviceAuthorizationInfo, DeviceAuthorizationPoll,
    DeviceAuthorizationStart, PendingDeviceAuthorization, PendingOAuth, TokenResponse,
};
use crate::model::OAuthTokens;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use chrono::Utc;
use rand::{rngs::OsRng, RngCore};
use reqwest::{header::AUTHORIZATION, RequestBuilder, Response, StatusCode};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use url::Url;

impl CloudClient {
    fn authorization_for_redirect(
        &self,
        redirect_uri: &str,
    ) -> Result<(Url, PendingOAuth), CloudError> {
        if self.config.client_id.is_empty() {
            return Err(CloudError::NotConfigured);
        }
        let mut verifier_bytes = [0_u8; 32];
        let mut state_bytes = [0_u8; 24];
        OsRng.fill_bytes(&mut verifier_bytes);
        OsRng.fill_bytes(&mut state_bytes);
        let verifier = URL_SAFE_NO_PAD.encode(verifier_bytes);
        let state = URL_SAFE_NO_PAD.encode(state_bytes);
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        let mut url =
            Url::parse(&self.config.authorize_url).map_err(|_| CloudError::NotConfigured)?;
        url.query_pairs_mut()
            .append_pair("response_type", "code")
            .append_pair("client_id", &self.config.client_id)
            .append_pair("redirect_uri", redirect_uri)
            .append_pair("scope", "openid offline_access")
            .append_pair("code_challenge", &challenge)
            .append_pair("code_challenge_method", "S256")
            .append_pair("state", &state);
        Ok((url, PendingOAuth { verifier, state }))
    }

    pub fn authorization(&self) -> Result<(Url, PendingOAuth), CloudError> {
        self.authorization_for_redirect(&self.config.redirect_uri)
    }

    pub async fn begin_device_authorization(
        &self,
    ) -> Result<(DeviceAuthorizationInfo, PendingDeviceAuthorization), CloudError> {
        let (_url, oauth) = self.authorization_for_redirect(&self.config.device_redirect_uri)?;
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(oauth.verifier.as_bytes()));
        let response = self
            .http
            .post(format!(
                "{}/v1/sync/device-auth/requests",
                self.config.api_url
            ))
            .json(&json!({ "state": oauth.state, "codeChallenge": challenge }))
            .send()
            .await
            .map_err(|_| CloudError::Unreachable)?;
        if !response.status().is_success() {
            log_http_failure("device authorization request", response).await;
            return Err(CloudError::OAuth);
        }
        let started: DeviceAuthorizationStart =
            response.json().await.map_err(|_| CloudError::OAuth)?;
        Ok((
            DeviceAuthorizationInfo {
                user_code: started.user_code,
                verification_uri: started.verification_uri,
                expires_in: started.expires_in,
            },
            PendingDeviceAuthorization {
                oauth,
                request_id: started.request_id,
                poll_token: started.poll_token,
            },
        ))
    }

    pub async fn complete_authorization(
        &self,
        callback: &str,
        pending: PendingOAuth,
    ) -> Result<(), CloudError> {
        let url = Url::parse(callback).map_err(|_| CloudError::OAuth)?;
        let redirect_uri = Url::parse(&self.config.redirect_uri).map_err(|_| CloudError::OAuth)?;
        if url.scheme() != redirect_uri.scheme()
            || url.host_str() != redirect_uri.host_str()
            || url.path() != redirect_uri.path()
        {
            return Err(CloudError::OAuth);
        }
        let parameters: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
        if parameters.get("state") != Some(&pending.state) || parameters.contains_key("error") {
            return Err(CloudError::OAuth);
        }
        let code = parameters.get("code").ok_or(CloudError::OAuth)?;
        self.exchange_authorization_code(code, &pending, &self.config.redirect_uri)
            .await
    }

    async fn exchange_authorization_code(
        &self,
        code: &str,
        pending: &PendingOAuth,
        redirect_uri: &str,
    ) -> Result<(), CloudError> {
        let response = self
            .http
            .post(&self.config.token_url)
            .form(&[
                ("grant_type", "authorization_code"),
                ("client_id", self.config.client_id.as_str()),
                ("redirect_uri", redirect_uri),
                ("code", code),
                ("code_verifier", pending.verifier.as_str()),
            ])
            .send()
            .await
            .map_err(|_| CloudError::Unreachable)?;
        if !response.status().is_success() {
            log_http_failure("authorization code exchange", response).await;
            return Err(CloudError::OAuth);
        }
        let token: TokenResponse = response.json().await.map_err(|_| CloudError::OAuth)?;
        self.credentials
            .save_tokens(&OAuthTokens {
                access_token: token.access_token,
                refresh_token: token.refresh_token,
                expires_at: Utc::now().timestamp() + token.expires_in.unwrap_or(3600),
            })
            .map_err(|_| CloudError::OAuth)
    }

    pub async fn poll_device_authorization(
        &self,
        pending: &PendingDeviceAuthorization,
    ) -> Result<Option<()>, CloudError> {
        let response = self
            .http
            .post(format!(
                "{}/v1/sync/device-auth/requests/{}/poll",
                self.config.api_url, pending.request_id
            ))
            .json(&json!({ "pollToken": pending.poll_token }))
            .send()
            .await
            .map_err(|_| CloudError::Unreachable)?;
        if response.status() == StatusCode::ACCEPTED {
            return Ok(None);
        }
        if response.status() == StatusCode::CONFLICT {
            return Err(CloudError::DeviceAuthorizationRejected);
        }
        if !response.status().is_success() {
            log_http_failure("device authorization poll", response).await;
            return Err(CloudError::OAuth);
        }
        let result: DeviceAuthorizationPoll =
            response.json().await.map_err(|_| CloudError::OAuth)?;
        if result.status != "authorized" {
            return Ok(None);
        }
        let code = result.authorization_code.ok_or(CloudError::OAuth)?;
        self.exchange_authorization_code(&code, &pending.oauth, &self.config.device_redirect_uri)
            .await
            .map_err(|error| match error {
                CloudError::OAuth => CloudError::DeviceAuthorizationExchangeFailed,
                other => other,
            })?;
        Ok(Some(()))
    }

    async fn access_token(&self) -> Result<String, CloudError> {
        self.refresh_access_token(None).await
    }

    async fn refresh_access_token(
        &self,
        rejected_token: Option<&str>,
    ) -> Result<String, CloudError> {
        // All background workers share this client. Re-read the keychain after
        // acquiring the lock so only one worker refreshes a given token.
        let _guard = self.refresh_lock.lock().await;
        let mut tokens = self
            .credentials
            .tokens()
            .map_err(|_| CloudError::OAuth)?
            .ok_or(CloudError::ReauthRequired)?;
        if rejected_token.is_some_and(|token| token != tokens.access_token)
            || (rejected_token.is_none() && tokens.expires_at > Utc::now().timestamp() + 60)
        {
            return Ok(tokens.access_token);
        }
        let refresh = tokens
            .refresh_token
            .clone()
            .ok_or(CloudError::ReauthRequired)?;
        let response = self
            .http
            .post(&self.config.token_url)
            .form(&[
                ("grant_type", "refresh_token"),
                ("client_id", self.config.client_id.as_str()),
                ("refresh_token", refresh.as_str()),
            ])
            .send()
            .await
            .map_err(|_| CloudError::Unreachable)?;
        if !response.status().is_success() {
            let status = response.status();
            let error = response.json::<Value>().await.ok().and_then(|body| {
                body.get("error")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            });
            eprintln!(
                "token refresh: HTTP {status} oauth_error={}",
                if error.as_deref() == Some("invalid_grant") {
                    "invalid_grant"
                } else {
                    "other"
                }
            );
            return Err(if error.as_deref() == Some("invalid_grant") {
                CloudError::ReauthRequired
            } else if status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error() {
                CloudError::Unreachable
            } else {
                CloudError::OAuth
            });
        }
        let refreshed: TokenResponse = response.json().await.map_err(|_| CloudError::OAuth)?;
        tokens.access_token = refreshed.access_token;
        tokens.refresh_token = refreshed.refresh_token.or(Some(refresh));
        tokens.expires_at = Utc::now().timestamp() + refreshed.expires_in.unwrap_or(3600);
        self.credentials
            .save_tokens(&tokens)
            .map_err(|_| CloudError::OAuth)?;
        Ok(tokens.access_token)
    }

    pub(super) async fn authorized(
        &self,
        method: reqwest::Method,
        path: &str,
    ) -> Result<reqwest::RequestBuilder, CloudError> {
        let token = self.access_token().await?;
        Ok(self
            .http
            .request(method, format!("{}{}", self.config.api_url, path))
            .bearer_auth(token))
    }

    pub(super) async fn send_authorized(
        &self,
        request: RequestBuilder,
    ) -> Result<Response, CloudError> {
        let retry = request.try_clone().ok_or(CloudError::Rejected)?;
        let attempted_token = retry
            .try_clone()
            .ok_or(CloudError::Rejected)?
            .build()
            .map_err(|_| CloudError::Rejected)?
            .headers()
            .get(AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "))
            .ok_or(CloudError::OAuth)?
            .to_string();
        let response = request.send().await.map_err(|_| CloudError::Unreachable)?;
        if response.status() != StatusCode::UNAUTHORIZED {
            return Ok(response);
        }
        let code = response.json::<Value>().await.ok().and_then(|body| {
            body.pointer("/details/code")
                .and_then(Value::as_str)
                .map(str::to_string)
        });
        if code.as_deref() == Some("SYNC_DEVICE_REVOKED") {
            return Err(CloudError::Revoked);
        }
        if code.as_deref() != Some("SYNC_ACCESS_TOKEN_INVALID") {
            return Err(CloudError::AuthenticationRejected);
        }
        // A stale access token can be rejected before its local expiry. One
        // forced refresh and retry distinguishes that from a revoked device.
        let token = self.refresh_access_token(Some(&attempted_token)).await?;
        let response = retry
            .bearer_auth(token)
            .send()
            .await
            .map_err(|_| CloudError::Unreachable)?;
        if response.status() == StatusCode::UNAUTHORIZED {
            let code = response.json::<Value>().await.ok().and_then(|body| {
                body.pointer("/details/code")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            });
            return Err(if code.as_deref() == Some("SYNC_DEVICE_REVOKED") {
                CloudError::Revoked
            } else {
                CloudError::AuthenticationRejected
            });
        }
        Ok(response)
    }
}
