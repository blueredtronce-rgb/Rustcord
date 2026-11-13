//! Profile-cosmetics reads (docs/COSMETICS_NOTES.md §5, §6, §10, §11): one collectibles product
//! (effect / frame config), the games of profile widgets, and my connections (list, visibility,
//! remove). Each call is one user action (a profile or the Connections page opened, a toggle
//! clicked); nothing is retried on 4xx, nothing cosmetic is ever written to the account.
//!
//! OWNERSHIP: cosmetics-display engineer.

use super::{ApiClient, ApiError};
use crate::model::{CollectibleProduct, ConnectedAccount, Game, Id};
use reqwest::{Method, Url};
use serde_json::{Map, Value};

/// `GET /games` takes at most 25 ids.
pub const MAX_GAME_IDS: usize = 25;

impl ApiClient {
    /// `GET /collectibles-products/{sku}` (works without a token too).
    pub async fn collectible_product(&self, sku_id: Id) -> Result<CollectibleProduct, ApiError> {
        self.send_json(Method::GET, Self::url(&["collectibles-products", &sku_id.to_string()]), None).await
    }

    /// `GET /games?game_ids=<id>&game_ids=<id>…` (1-25 ids).
    pub async fn games(&self, ids: &[Id]) -> Result<Vec<Game>, ApiError> {
        self.send_json(Method::GET, games_url(ids), None).await
    }

    /// `GET /users/@me/connections`.
    pub async fn connections(&self) -> Result<Vec<ConnectedAccount>, ApiError> {
        self.send_json(Method::GET, Self::url(&["users", "@me", "connections"]), None).await
    }

    /// `PATCH /users/@me/connections/{type}/{id}` with the set toggles (`visibility` 0/1,
    /// `show_activity`).
    pub async fn update_connection(&self, kind: &str, id: &str, visibility: Option<bool>, show_activity: Option<bool>) -> Result<(), ApiError> {
        let mut body = Map::new();
        if let Some(v) = visibility {
            body.insert("visibility".into(), Value::from(u8::from(v)));
        }
        if let Some(v) = show_activity {
            body.insert("show_activity".into(), Value::from(v));
        }
        self.send(Method::PATCH, Self::url(&["users", "@me", "connections", kind, id]), Some(Value::Object(body))).await.map(drop)
    }

    /// `DELETE /users/@me/connections/{type}/{id}` -> 204 (also leaves guilds joined through it).
    pub async fn remove_connection(&self, kind: &str, id: &str) -> Result<(), ApiError> {
        self.send(Method::DELETE, Self::url(&["users", "@me", "connections", kind, id]), None).await.map(drop)
    }
}

fn games_url(ids: &[Id]) -> Url {
    let mut url = ApiClient::url(&["games"]);
    {
        let mut q = url.query_pairs_mut();
        for id in ids.iter().take(MAX_GAME_IDS) {
            q.append_pair("game_ids", &id.to_string());
        }
    }
    url
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routes() {
        let u = games_url(&(1..=30).map(Id).collect::<Vec<_>>());
        assert!(u.as_str().ends_with("/games?game_ids=1&game_ids=2&game_ids=3&game_ids=4&game_ids=5&game_ids=6&game_ids=7&game_ids=8&game_ids=9&game_ids=10&game_ids=11&game_ids=12&game_ids=13&game_ids=14&game_ids=15&game_ids=16&game_ids=17&game_ids=18&game_ids=19&game_ids=20&game_ids=21&game_ids=22&game_ids=23&game_ids=24&game_ids=25"), "{u}");
        // Path segments are encoded: a hostile connection id cannot leave the route.
        let c = ApiClient::url(&["users", "@me", "connections", "domain", "../x?y"]);
        assert!(c.as_str().ends_with("/users/@me/connections/domain/..%2Fx%3Fy"), "{c}");
    }
}
