use std::time::Duration;

use anyhow::{Context, Result, bail};
use reqwest::StatusCode;
use serde::de::DeserializeOwned;
use tokio::time::sleep;

const MAX_RETRIES: usize = 2;
const BASE_BACKOFF_MS: u64 = 250;

pub fn default_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent(user_agent())
        .build()
        .context("failed to build HTTP client")
}

pub fn github_client() -> Result<reqwest::Client> {
    let mut headers = reqwest::header::HeaderMap::new();
    if let Ok(token) = std::env::var("GITHUB_TOKEN") {
        let value = format!("Bearer {token}")
            .parse()
            .context("invalid GITHUB_TOKEN header value")?;
        headers.insert(reqwest::header::AUTHORIZATION, value);
    }

    reqwest::Client::builder()
        .user_agent(user_agent())
        .default_headers(headers)
        .build()
        .context("failed to build HTTP client")
}

fn user_agent() -> String {
    format!("ruckup/{}", env!("CARGO_PKG_VERSION"))
}

pub async fn get_json_with_retries<T, F>(mut make_request: F) -> Result<T>
where
    T: DeserializeOwned,
    F: FnMut() -> reqwest::RequestBuilder,
{
    for attempt in 0..=MAX_RETRIES {
        match make_request().send().await {
            Ok(response) => {
                let status = response.status();
                if status.is_success() {
                    return response
                        .json::<T>()
                        .await
                        .context("failed to parse JSON response");
                }

                if should_retry_status(status) && attempt < MAX_RETRIES {
                    sleep(backoff_delay(attempt)).await;
                    continue;
                }

                let body = response.text().await.unwrap_or_default();
                bail!("request failed with status {status}: {body}");
            }
            Err(err) => {
                if (err.is_timeout() || err.is_connect()) && attempt < MAX_RETRIES {
                    sleep(backoff_delay(attempt)).await;
                    continue;
                }
                return Err(err).context("request failed");
            }
        }
    }

    bail!("request failed after retry attempts")
}

fn backoff_delay(attempt: usize) -> Duration {
    Duration::from_millis(BASE_BACKOFF_MS * (attempt as u64 + 1))
}

fn should_retry_status(status: StatusCode) -> bool {
    status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error()
}
