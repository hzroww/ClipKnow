//! 只读管理接口。权限来自签名范围，路径中的 user 只是查询目标。
use super::{ApiError, AppState, auth::AdminPrincipal, join_failed};
use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
};
use serde::Deserialize;
use serde_json::Value;
use std::sync::Arc;

#[derive(Deserialize)]
pub(super) struct Page {
    limit: Option<usize>,
    offset: Option<usize>,
}
impl Page {
    fn bounds(&self) -> std::result::Result<(usize, usize), ApiError> {
        let limit = self.limit.unwrap_or(30);
        let offset = self.offset.unwrap_or(0);
        if !(1..=100).contains(&limit) || offset > i64::MAX as usize {
            return Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                "bad_request",
                "分页参数不合法，limit 应为 1–100",
            ));
        }
        Ok((limit, offset))
    }
}
fn found(value: Option<Value>) -> std::result::Result<Json<Value>, ApiError> {
    value
        .map(Json)
        .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "not_found", "没有找到这条记录"))
}
pub(super) async fn sessions(
    State(st): State<Arc<AppState>>,
    _admin: AdminPrincipal,
    Path(user): Path<String>,
    Query(page): Query<Page>,
) -> std::result::Result<Json<Value>, ApiError> {
    let (limit, offset) = page.bounds()?;
    Ok(Json(
        tokio::task::spawn_blocking(move || st.read().admin_sessions(&user, limit, offset))
            .await
            .map_err(join_failed)??,
    ))
}
pub(super) async fn turns(
    State(st): State<Arc<AppState>>,
    _admin: AdminPrincipal,
    Path(session): Path<String>,
    Query(page): Query<Page>,
) -> std::result::Result<Json<Value>, ApiError> {
    let (limit, offset) = page.bounds()?;
    found(
        tokio::task::spawn_blocking(move || st.read().admin_turns(&session, limit, offset))
            .await
            .map_err(join_failed)??,
    )
}
pub(super) async fn turn(
    State(st): State<Arc<AppState>>,
    _admin: AdminPrincipal,
    Path(turn): Path<String>,
) -> std::result::Result<Json<Value>, ApiError> {
    found(
        tokio::task::spawn_blocking(move || st.read().admin_turn(&turn))
            .await
            .map_err(join_failed)??,
    )
}
pub(super) async fn raw(
    State(st): State<Arc<AppState>>,
    _admin: AdminPrincipal,
    Path((turn, item)): Path<(String, String)>,
) -> std::result::Result<Json<Value>, ApiError> {
    found(
        tokio::task::spawn_blocking(move || st.read().admin_raw(&turn, &item))
            .await
            .map_err(join_failed)??,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        serve::{auth, testserver},
        store::sqlite::SqliteStore,
    };
    use jsonwebtoken::{Algorithm, EncodingKey, Header};
    use serde_json::json;

    #[test]
    fn every_admin_http_route_rejects_ordinary_tokens() {
        let secret = b"admin-http-test-secret-longer-than-32";
        let state = Arc::new(AppState::new(
            SqliteStore::in_memory().unwrap(),
            ":memory:".into(),
            auth::Verifier::new(secret),
        ));
        let srv = testserver::spawn(state);
        let now = crate::content::model::now_ts();
        let mut claims =
            json!({"sub":"owner","iss":auth::ISSUER,"aud":auth::AUDIENCE,"iat":now,"exp":now+120});
        let ordinary = jsonwebtoken::encode(
            &Header::new(Algorithm::HS256),
            &claims,
            &EncodingKey::from_secret(secret),
        )
        .unwrap();
        claims["scope"] = json!("admin:read");
        let admin = jsonwebtoken::encode(
            &Header::new(Algorithm::HS256),
            &claims,
            &EncodingKey::from_secret(secret),
        )
        .unwrap();
        let client = reqwest::blocking::Client::new();
        for path in [
            "/internal/admin/users/u1/sessions",
            "/internal/admin/sessions/s1/turns",
            "/internal/admin/turns/t1",
            "/internal/admin/turns/t1/items/i1/raw",
        ] {
            assert_eq!(client.get(srv.url(path)).send().unwrap().status(), 401);
            assert_eq!(
                client
                    .get(srv.url(path))
                    .bearer_auth(&ordinary)
                    .send()
                    .unwrap()
                    .status(),
                403
            );
            let response = client
                .get(srv.url(path))
                .bearer_auth(&admin)
                .send()
                .unwrap();
            assert_eq!(
                response.status(),
                if path.contains("/users/") { 200 } else { 404 }
            );
        }
        let r = client
            .get(srv.url("/internal/admin/users/u1/sessions?limit=0"))
            .bearer_auth(admin)
            .send()
            .unwrap();
        assert_eq!(r.status(), 400);
    }
}
