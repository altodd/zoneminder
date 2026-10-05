//! Long-lived API tokens (Home Assistant, federation peers, scripts) and
//! users changing their own password.
mod common;
use common::*;
use serde_json::json;

struct World {
    fx: Fixture,
    front: i64,
    back: i64,
    admin_id: i64,
    viewer_id: i64,
    admin: String,
    viewer: String,
}

fn world() -> World {
    let fx = Fixture::new();
    let front = fx.add_camera("front");
    let back = fx.add_camera("back");
    let admin_id = fx.add_admin("admin", "password123");
    let viewer_id = fx.add_viewer("guest", "password123", &[front]);
    World { admin: fx.token_for(admin_id), viewer: fx.token_for(viewer_id), fx, front, back, admin_id, viewer_id }
}

async fn login(r: &axum::Router, user: &str, pw: &str) -> Resp {
    call(r, "POST", "/api/login", None, Some(json!({"username": user, "password": pw}))).await
}

/// An admin issues a named token for any user; it acts as that user (a
/// viewer token sees only the viewer's cameras), shows up in the list by
/// name and hint (never the token itself), records its last use, and stops
/// working the moment it is revoked.
#[tokio::test]
async fn admin_issues_lists_and_revokes_tokens_for_any_user() {
    let w = world();
    let r = w.fx.router();
    let res = call(&r, "POST", "/api/tokens", Some(&w.admin), Some(json!({"name": "barn peer", "user_id": w.viewer_id, "expires_days": 90}))).await;
    assert_eq!(res.status, 200, "{}", res.text());
    let t = res.json();
    let tok = t["token"].as_str().unwrap().to_string();
    assert!(tok.starts_with("zmng_"), "{tok}");
    assert_eq!(t["name"], "barn peer");
    assert_eq!(t["user_id"], w.viewer_id);
    let exp = t["expires_at"].as_i64().unwrap();
    let now = chrono::Utc::now().timestamp();
    assert!((exp - (now + 90 * 86400)).abs() < 60, "{exp}");
    // it is the viewer: one camera, no admin surface
    let cams = get(&r, "/api/cameras", &tok).await.json();
    assert_eq!(cams.as_array().unwrap().len(), 1);
    assert_eq!(cams[0]["id"], w.front);
    assert_eq!(get(&r, "/api/users", &tok).await.status, 403);
    assert_eq!(get(&r, &format!("/api/cameras/{}/timeline?start=0&end=1000", w.back), &tok).await.status, 404);
    // the ZoneMinder API and media take it as ?token= too
    assert_eq!(call(&r, "GET", &format!("/zm/api/monitors.json?token={tok}"), None, None).await.status, 200);
    // listed by name, user and hint; the token itself is never shown again
    let list = get(&r, "/api/tokens", &w.admin).await;
    assert!(!list.text().contains(&tok[5..]), "list leaks the token");
    let row = list.json().as_array().unwrap().iter().find(|x| x["name"] == "barn peer").unwrap().clone();
    assert_eq!(row["username"], "guest");
    assert_eq!(row["hint"].as_str().unwrap(), &tok[..10]);
    assert!(row["last_used_at"].as_i64().is_some(), "use is recorded: {row}");
    // stored as a hash
    let conn = rusqlite::Connection::open(&w.fx.cfg.db_path).unwrap();
    let plain: i64 = conn.query_row("SELECT COUNT(*) FROM api_token WHERE token_hash=?1 OR hint=?1", [&tok], |r| r.get(0)).unwrap();
    assert_eq!(plain, 0);
    // revoke: gone at once
    let id = row["id"].as_i64().unwrap();
    assert_eq!(call(&r, "DELETE", &format!("/api/tokens/{id}"), Some(&w.admin), None).await.status, 200);
    assert_eq!(get(&r, "/api/cameras", &tok).await.status, 401);
    assert_eq!(call(&r, "DELETE", &format!("/api/tokens/{id}"), Some(&w.admin), None).await.status, 404);
}

/// Viewers cannot issue tokens, see only their own, and can revoke only
/// their own. A token without a body is the caller's own, named "API token".
#[tokio::test]
async fn viewers_see_and_revoke_only_their_own_tokens() {
    let w = world();
    let r = w.fx.router();
    assert_eq!(call(&r, "POST", "/api/tokens", Some(&w.viewer), Some(json!({"name": "mine"}))).await.status, 403);
    let mine = call(&r, "POST", "/api/tokens", Some(&w.admin), Some(json!({"name": "guest phone", "user_id": w.viewer_id}))).await.json();
    let admins = call(&r, "POST", "/api/tokens", Some(&w.admin), None).await.json();
    assert_eq!(admins["name"], "API token");
    assert_eq!(admins["user_id"], w.admin_id);
    assert!(admins["expires_at"].is_null());
    let list = get(&r, "/api/tokens", &w.viewer).await.json();
    assert_eq!(list.as_array().unwrap().len(), 1, "{list}");
    assert_eq!(list[0]["name"], "guest phone");
    assert_eq!(get(&r, "/api/tokens", &w.admin).await.json().as_array().unwrap().len(), 2);
    assert_eq!(call(&r, "DELETE", &format!("/api/tokens/{}", admins["id"]), Some(&w.viewer), None).await.status, 404);
    assert_eq!(call(&r, "DELETE", &format!("/api/tokens/{}", mine["id"]), Some(&w.viewer), None).await.status, 200);
    // bad input
    assert_eq!(call(&r, "POST", "/api/tokens", Some(&w.admin), Some(json!({"name": "x", "user_id": 999}))).await.status, 400);
    assert_eq!(call(&r, "POST", "/api/tokens", Some(&w.admin), Some(json!({"name": "", "user_id": w.viewer_id}))).await.status, 400);
    assert_eq!(call(&r, "POST", "/api/tokens", Some(&w.admin), Some(json!({"name": "x", "expires_days": 0}))).await.status, 400);
}

/// An expired token is refused; deleting a user removes their tokens.
#[tokio::test]
async fn expired_tokens_fail_and_tokens_die_with_their_user() {
    let w = world();
    let r = w.fx.router();
    w.fx.db.create_api_token(w.viewer_id, "old", "zmng_expired", Some(chrono::Utc::now().timestamp() - 10), Some(w.admin_id)).unwrap();
    assert_eq!(get(&r, "/api/me", "zmng_expired").await.status, 401);
    let t = call(&r, "POST", "/api/tokens", Some(&w.admin), Some(json!({"name": "ha", "user_id": w.viewer_id}))).await.json();
    let tok = t["token"].as_str().unwrap();
    assert_eq!(get(&r, "/api/me", tok).await.status, 200);
    assert_eq!(call(&r, "DELETE", &format!("/api/users/{}", w.viewer_id), Some(&w.admin), None).await.status, 200);
    assert_eq!(get(&r, "/api/me", tok).await.status, 401);
    assert!(get(&r, "/api/tokens", &w.admin).await.json().as_array().unwrap().is_empty());
}

/// Tokens made by older builds were 10-year sessions: at upgrade they become
/// listed, revocable API tokens and keep working.
#[tokio::test]
async fn legacy_long_lived_sessions_become_api_tokens() {
    let w = world();
    let legacy = zmng::api::new_token();
    w.fx.db.create_session(w.admin_id, &legacy, 10 * 365 * 86400).unwrap();
    let short = zmng::api::new_token();
    w.fx.db.create_session(w.admin_id, &short, 14 * 86400).unwrap();
    // a database from an older build: no record of the move, and a session
    // left behind by a user deleted while foreign keys were not enforced
    {
        let conn = rusqlite::Connection::open(&w.fx.cfg.db_path).unwrap();
        conn.execute_batch("DELETE FROM meta WHERE key='legacy_tokens_moved'; PRAGMA foreign_keys=OFF; INSERT INTO session(token,user_id,created_at,expires_at) VALUES('orphan',999,0,315360000);").unwrap();
    }
    // reopen = upgrade
    let db = zmng::db::Db::open(&w.fx.cfg.db_path).unwrap();
    let app = zmng::api::App::new(w.fx.cfg.clone(), db, w.fx.hub.clone(), w.fx.bus.clone());
    let r = zmng::api::router(app);
    assert_eq!(get(&r, "/api/me", &legacy).await.status, 200);
    assert_eq!(get(&r, "/api/me", &short).await.status, 200);
    let list = get(&r, "/api/tokens", &w.admin).await.json();
    assert_eq!(list.as_array().unwrap().len(), 1, "{list}");
    assert_eq!(list[0]["hint"].as_str().unwrap(), &legacy[..10]);
    assert!(list[0]["name"].as_str().unwrap().contains("earlier"), "{list}");
    let conn = rusqlite::Connection::open(&w.fx.cfg.db_path).unwrap();
    let left: i64 = conn.query_row("SELECT COUNT(*) FROM session WHERE token=?1", [&legacy], |r| r.get(0)).unwrap();
    assert_eq!(left, 0, "the session row is replaced, not duplicated");
    let orphan: i64 = conn.query_row("SELECT COUNT(*) FROM session WHERE token='orphan'", [], |r| r.get(0)).unwrap();
    assert_eq!(orphan, 0, "dropped, not an error at startup");
    // once only: a later 10-year session (none are issued now) stays a session
    let later = zmng::api::new_token();
    w.fx.db.create_session(w.admin_id, &later, 10 * 365 * 86400).unwrap();
    drop(zmng::db::Db::open(&w.fx.cfg.db_path).unwrap());
    let kept: i64 = conn.query_row("SELECT COUNT(*) FROM session WHERE token=?1", [&later], |r| r.get(0)).unwrap();
    assert_eq!(kept, 1);
    assert_eq!(call(&r, "DELETE", &format!("/api/tokens/{}", list[0]["id"]), Some(&w.admin), None).await.status, 200);
    assert_eq!(get(&r, "/api/me", &legacy).await.status, 401);
}

/// Anyone can change their own password with the current one. Their other
/// sign-ins (browsers, zmNinjaNg) end; the one used to change it and their
/// API tokens keep working. An admin resetting someone's password ends that
/// user's sign-ins too.
#[tokio::test]
async fn users_change_their_own_password() {
    let w = world();
    let r = w.fx.router();
    let other_browser = w.fx.token_for(w.viewer_id);
    let api_tok = call(&r, "POST", "/api/tokens", Some(&w.admin), Some(json!({"name": "ha", "user_id": w.viewer_id}))).await.json()["token"].as_str().unwrap().to_string();
    let url = "/api/me/password";
    assert_eq!(call(&r, "POST", url, Some(&w.viewer), Some(json!({"current": "wrong-password", "new": "a-new-password"}))).await.status, 403);
    assert_eq!(call(&r, "POST", url, Some(&w.viewer), Some(json!({"current": "password123", "new": "short"}))).await.status, 400);
    assert_eq!(call(&r, "POST", url, Some(&w.viewer), Some(json!({"current": "password123", "new": "password123"}))).await.status, 400);
    let res = call(&r, "POST", url, Some(&w.viewer), Some(json!({"current": "password123", "new": "a-new-password"}))).await;
    assert_eq!(res.status, 200, "{}", res.text());
    assert_eq!(login(&r, "guest", "password123").await.status, 401);
    assert_eq!(login(&r, "guest", "a-new-password").await.status, 200);
    assert_eq!(get(&r, "/api/me", &w.viewer).await.status, 200, "the session that changed it stays signed in");
    assert_eq!(get(&r, "/api/me", &other_browser).await.status, 401, "other sign-ins end");
    assert_eq!(get(&r, "/api/me", &api_tok).await.status, 200, "API tokens are separate credentials");
    // an API token alone cannot change the password without the current one
    assert_eq!(call(&r, "POST", url, Some(&api_tok), Some(json!({"current": "nope-nope", "new": "another-password"}))).await.status, 403);
    // admin reset ends the user's sign-ins
    let fresh = login(&r, "guest", "a-new-password").await.json()["token"].as_str().unwrap().to_string();
    assert_eq!(call(&r, "PATCH", &format!("/api/users/{}", w.viewer_id), Some(&w.admin), Some(json!({"password": "reset-by-admin"}))).await.status, 200);
    assert_eq!(get(&r, "/api/me", &fresh).await.status, 401);
    assert_eq!(get(&r, "/api/me", &w.admin).await.status, 200);
    assert_eq!(login(&r, "guest", "reset-by-admin").await.status, 200);
    // an admin's own password goes through the current-password check too
    let res = call(&r, "PATCH", &format!("/api/users/{}", w.admin_id), Some(&w.admin), Some(json!({"password": "taken-over-pw"}))).await;
    assert_eq!(res.status, 400, "{}", res.text());
    assert!(res.text().contains("/api/me/password"), "{}", res.text());
    assert_eq!(login(&r, "admin", "password123").await.status, 200);
}


/// An account imported from ZoneMinder keeps its PHP bcrypt hash ($2y$): the
/// old password logs in, the hash is upgraded to argon2 on that first login,
/// the next login verifies the argon2 hash, and a wrong password never passes.
#[tokio::test]
async fn zoneminder_bcrypt_hashes_log_in_and_upgrade_to_argon2() {
    let fx = Fixture::new();
    fx.add_admin("boss", "bosspassword");
    let zm_hash = bcrypt::hash("OldZmSecret!", 4).unwrap();
    assert!(zm_hash.starts_with("$2b$") || zm_hash.starts_with("$2y$"));
    let id = fx.db.add_user("kstabler", &zm_hash, "viewer").unwrap();
    let r = fx.router();
    assert_eq!(login(&r, "kstabler", "wrong-password").await.status, 401);
    assert_eq!(login(&r, "kstabler", "OldZmSecret!").await.status, 200, "the ZoneMinder password works");
    let stored = fx.db.user_by_name("kstabler").unwrap().unwrap().pass_hash;
    assert!(stored.starts_with("$argon2"), "upgraded on first login: {stored}");
    assert_eq!(login(&r, "kstabler", "OldZmSecret!").await.status, 200, "and still works afterwards");
    assert_eq!(login(&r, "kstabler", "wrong-password").await.status, 401);
    assert_eq!(fx.db.user_by_name("kstabler").unwrap().unwrap().id, id);
}
