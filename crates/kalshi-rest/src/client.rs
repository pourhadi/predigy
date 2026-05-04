//! HTTP client wrapping reqwest with optional Kalshi RSA-PSS auth.

use crate::auth::Signer;
use crate::error::Error;
use crate::types::{
    BatchCancelOrdersRequest, BatchCancelOrdersResponse, CancelOrderResponse, CreateOrderRequest,
    CreateOrderResponse, FillsResponse, MarketDetailResponse, MarketsResponse, OrderbookResponse,
    PositionsResponse, SeriesListResponse,
};
use predigy_book::Snapshot;
use predigy_core::price::Price;
use reqwest::Method;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use std::time::Duration;
use url::Url;

const DEFAULT_BASE: &str = "https://api.elections.kalshi.com/trade-api/v2";
const DEFAULT_TIMEOUT_SECS: u64 = 10;

// HeaderName::from_static requires lowercase ASCII.
const H_KEY: HeaderName = HeaderName::from_static("kalshi-access-key");
const H_TS: HeaderName = HeaderName::from_static("kalshi-access-timestamp");
const H_SIG: HeaderName = HeaderName::from_static("kalshi-access-signature");

#[derive(Debug)]
pub struct Client {
    http: reqwest::Client,
    base: Url,
    /// Path prefix relative to host, e.g. `/trade-api/v2`. Used when building
    /// the signature payload (Kalshi signs the full path from API root).
    path_prefix: String,
    signer: Option<Signer>,
}

impl Client {
    /// Build a client against the public Kalshi API with no auth (read-only
    /// public endpoints only).
    pub fn public() -> Result<Self, Error> {
        Self::with_base(DEFAULT_BASE, None)
    }

    /// Build a client with a signer (authenticated endpoints).
    pub fn authed(signer: Signer) -> Result<Self, Error> {
        Self::with_base(DEFAULT_BASE, Some(signer))
    }

    /// Build a client with a custom base URL (e.g. demo / sandbox).
    pub fn with_base(base_url: &str, signer: Option<Signer>) -> Result<Self, Error> {
        let base = Url::parse(base_url)?;
        let path_prefix = base.path().trim_end_matches('/').to_string();
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(DEFAULT_TIMEOUT_SECS))
            .user_agent("predigy/0.1")
            .build()?;
        Ok(Self {
            http,
            base,
            path_prefix,
            signer,
        })
    }

    fn build_url(&self, sub_path: &str) -> Result<Url, Error> {
        let trimmed = sub_path.trim_start_matches('/');
        let joined = format!("{}/{}", self.base.as_str().trim_end_matches('/'), trimmed);
        Ok(Url::parse(&joined)?)
    }

    fn sign_headers(&self, method: &Method, sub_path: &str) -> Result<HeaderMap, Error> {
        let mut headers = HeaderMap::new();
        let Some(signer) = &self.signer else {
            return Ok(headers);
        };
        let full_path = format!("{}/{}", self.path_prefix, sub_path.trim_start_matches('/'));
        let (ts, sig) = signer.sign(method.as_str(), &full_path);
        let to_v = |s: &str| HeaderValue::from_str(s).map_err(|e| Error::Auth(e.to_string()));
        headers.insert(H_KEY, to_v(signer.key_id())?);
        headers.insert(H_TS, to_v(&ts)?);
        headers.insert(H_SIG, to_v(&sig)?);
        Ok(headers)
    }

    async fn get_json<T: serde::de::DeserializeOwned>(
        &self,
        sub_path: &str,
        query: &[(&str, String)],
    ) -> Result<T, Error> {
        let url = self.build_url(sub_path)?;
        let headers = self.sign_headers(&Method::GET, sub_path)?;
        let resp = self
            .http
            .get(url)
            .headers(headers)
            .query(query)
            .send()
            .await?;
        Self::decode_response(resp).await
    }

    async fn post_json<B: serde::Serialize, T: serde::de::DeserializeOwned>(
        &self,
        sub_path: &str,
        body: &B,
    ) -> Result<T, Error> {
        let url = self.build_url(sub_path)?;
        let headers = self.sign_headers(&Method::POST, sub_path)?;
        let resp = self
            .http
            .post(url)
            .headers(headers)
            .json(body)
            .send()
            .await?;
        Self::decode_response(resp).await
    }

    async fn delete_json<T: serde::de::DeserializeOwned>(
        &self,
        sub_path: &str,
        query: &[(&str, String)],
    ) -> Result<T, Error> {
        let url = self.build_url(sub_path)?;
        let headers = self.sign_headers(&Method::DELETE, sub_path)?;
        let resp = self
            .http
            .delete(url)
            .headers(headers)
            .query(query)
            .send()
            .await?;
        Self::decode_response(resp).await
    }

    /// `DELETE` with a JSON body. Required for Kalshi's V2 batch
    /// cancel endpoint, which is one of the few REST APIs in the
    /// wild that demands a body on `DELETE`.
    async fn delete_json_body<B: serde::Serialize, T: serde::de::DeserializeOwned>(
        &self,
        sub_path: &str,
        body: &B,
    ) -> Result<T, Error> {
        let url = self.build_url(sub_path)?;
        let headers = self.sign_headers(&Method::DELETE, sub_path)?;
        let resp = self
            .http
            .delete(url)
            .headers(headers)
            .json(body)
            .send()
            .await?;
        Self::decode_response(resp).await
    }

    async fn decode_response<T: serde::de::DeserializeOwned>(
        resp: reqwest::Response,
    ) -> Result<T, Error> {
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(Error::Api {
                status: status.as_u16(),
                body,
            });
        }
        let bytes = resp.bytes().await?;
        let parsed = serde_json::from_slice::<T>(&bytes)?;
        Ok(parsed)
    }

    /// `GET /markets` with optional filters.
    pub async fn list_markets(
        &self,
        status: Option<&str>,
        event_ticker: Option<&str>,
        limit: Option<u32>,
        cursor: Option<&str>,
    ) -> Result<MarketsResponse, Error> {
        let mut q = Vec::new();
        if let Some(s) = status {
            q.push(("status", s.to_string()));
        }
        if let Some(e) = event_ticker {
            q.push(("event_ticker", e.to_string()));
        }
        if let Some(l) = limit {
            q.push(("limit", l.to_string()));
        }
        if let Some(c) = cursor {
            q.push(("cursor", c.to_string()));
        }
        self.get_json("/markets", &q).await
    }

    /// `GET /markets?series_ticker=...` — Kalshi supports both
    /// `event_ticker` and `series_ticker` as filters; the latter
    /// matches every event in a series. Used by the weather-market
    /// scanner to avoid paginating the full universe of markets.
    pub async fn list_markets_in_series(
        &self,
        series_ticker: &str,
        status: Option<&str>,
        limit: Option<u32>,
        cursor: Option<&str>,
    ) -> Result<MarketsResponse, Error> {
        let mut q = vec![("series_ticker", series_ticker.to_string())];
        if let Some(s) = status {
            q.push(("status", s.to_string()));
        }
        if let Some(l) = limit {
            q.push(("limit", l.to_string()));
        }
        if let Some(c) = cursor {
            q.push(("cursor", c.to_string()));
        }
        self.get_json("/markets", &q).await
    }

    /// `GET /series` filtered by category. Kalshi's category names
    /// are stable strings (`"Climate and Weather"`, `"Politics"`,
    /// `"Sports"`, etc.). Returns the full list — not paginated.
    pub async fn list_series_by_category(
        &self,
        category: &str,
    ) -> Result<SeriesListResponse, Error> {
        let q = vec![("category", category.to_string())];
        // Some Kalshi accounts get `series: null` instead of an
        // empty array when there's no match; the type's
        // serde(default) handles that case.
        self.get_json("/series", &q).await
    }

    /// `GET /markets/{ticker}`.
    pub async fn market_detail(&self, ticker: &str) -> Result<MarketDetailResponse, Error> {
        self.get_json(&format!("/markets/{ticker}"), &[]).await
    }

    /// `GET /markets/{ticker}/orderbook` and convert to a `Snapshot` for the
    /// `book` crate. Kalshi's REST orderbook does not include a sequence
    /// number; we use 0 as a placeholder. Real sequencing comes from the WS
    /// `orderbook_snapshot` message which carries `seq`.
    pub async fn orderbook_snapshot(&self, ticker: &str) -> Result<Snapshot, Error> {
        let resp: OrderbookResponse = self
            .get_json(&format!("/markets/{ticker}/orderbook"), &[])
            .await?;
        let convert = |levels: Vec<[String; 2]>| -> Vec<(Price, u32)> {
            levels
                .into_iter()
                .filter_map(|[px_str, qty_str]| {
                    let px_dollars: f64 = px_str.parse().ok()?;
                    let cents_i = (px_dollars * 100.0).round() as i32;
                    let cents_u8 = u8::try_from(cents_i).ok()?;
                    let price = Price::from_cents(cents_u8).ok()?;
                    let qty_dollars: f64 = qty_str.parse().ok()?;
                    let q = qty_dollars.round() as i64;
                    if q <= 0 {
                        return None;
                    }
                    Some((price, u32::try_from(q).unwrap_or(0)))
                })
                .collect()
        };
        Ok(Snapshot {
            seq: 0,
            yes_bids: convert(resp.orderbook_fp.yes_dollars),
            no_bids: convert(resp.orderbook_fp.no_dollars),
        })
    }

    /// `GET /portfolio/positions` (auth required).
    pub async fn positions(&self) -> Result<PositionsResponse, Error> {
        if self.signer.is_none() {
            return Err(Error::Auth("positions endpoint requires a signer".into()));
        }
        self.get_json("/portfolio/positions", &[]).await
    }

    /// `POST /portfolio/events/orders` (auth required). V2 schema.
    pub async fn create_order(
        &self,
        req: &CreateOrderRequest,
    ) -> Result<CreateOrderResponse, Error> {
        if self.signer.is_none() {
            return Err(Error::Auth("create_order requires a signer".into()));
        }
        self.post_json("/portfolio/events/orders", req).await
    }

    /// `DELETE /portfolio/events/orders/{order_id}` (auth required).
    pub async fn cancel_order(&self, order_id: &str) -> Result<CancelOrderResponse, Error> {
        if self.signer.is_none() {
            return Err(Error::Auth("cancel_order requires a signer".into()));
        }
        self.delete_json(&format!("/portfolio/events/orders/{order_id}"), &[])
            .await
    }

    /// `DELETE /portfolio/events/orders/batched` (auth required).
    /// Cancels every venue order id in `request.orders` in one round
    /// trip. Per-order results are returned individually — Kalshi does
    /// not roll back the batch if some entries fail.
    pub async fn batch_cancel_orders(
        &self,
        request: &BatchCancelOrdersRequest,
    ) -> Result<BatchCancelOrdersResponse, Error> {
        if self.signer.is_none() {
            return Err(Error::Auth("batch_cancel_orders requires a signer".into()));
        }
        self.delete_json_body("/portfolio/events/orders/batched", request)
            .await
    }

    /// `GET /portfolio/fills` (auth required). Filter by `order_id`,
    /// `min_ts` (Unix seconds), and pagination `cursor` as needed.
    pub async fn list_fills(
        &self,
        order_id: Option<&str>,
        min_ts: Option<i64>,
        limit: Option<u32>,
        cursor: Option<&str>,
    ) -> Result<FillsResponse, Error> {
        if self.signer.is_none() {
            return Err(Error::Auth("list_fills requires a signer".into()));
        }
        let mut q = Vec::new();
        if let Some(o) = order_id {
            q.push(("order_id", o.to_string()));
        }
        if let Some(t) = min_ts {
            q.push(("min_ts", t.to_string()));
        }
        if let Some(l) = limit {
            q.push(("limit", l.to_string()));
        }
        if let Some(c) = cursor {
            q.push(("cursor", c.to_string()));
        }
        self.get_json("/portfolio/fills", &q).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_url_handles_leading_slash() {
        let c = Client::public().unwrap();
        assert_eq!(
            c.build_url("/markets").unwrap().as_str(),
            "https://api.elections.kalshi.com/trade-api/v2/markets"
        );
        assert_eq!(
            c.build_url("markets/X").unwrap().as_str(),
            "https://api.elections.kalshi.com/trade-api/v2/markets/X"
        );
    }

    #[test]
    fn public_client_has_no_signer() {
        let c = Client::public().unwrap();
        assert!(c.signer.is_none());
    }

    #[test]
    fn positions_requires_signer() {
        let c = Client::public().unwrap();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let err = rt.block_on(c.positions()).unwrap_err();
        assert!(matches!(err, Error::Auth(_)));
    }
}
