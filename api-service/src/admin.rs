use axum::extract::Query;
use axum::routing::put;
use axum::{Extension, Json, Router};
use reqwest_middleware::ClientWithMiddleware;
use serde::{Deserialize, Serialize};
use service_common::error::AppError;
use sqlx::PgPool;
use std::collections::{HashMap, HashSet};

pub(crate) fn router() -> Router {
    Router::new().route("/admin/players", put(update_players).get(get_players))
}

async fn update_players(
    Extension(client): Extension<ClientWithMiddleware>,
    Extension(pg_pool): Extension<PgPool>,
) -> Result<(), AppError> {
    let players = client
        .get("https://api.sleeper.app/v1/players/nfl")
        .send()
        .await?
        .error_for_status()?
        .json::<HashMap<String, SleeperPlayer>>()
        .await?;
    let positions = HashSet::from(["QB", "RB", "WR", "TE", "K", "DEF"]);

    for player in players.into_values().filter(|s| {
        s.position
            .as_ref()
            .map_or(false, |p| positions.contains(p.as_str()))
    }) {
        sqlx::query(
            r#"
insert into
    players(id, first_name, last_name, active, position, team, status, injury_status)
values
    ($1, $2, $3, $4, $5, $6, $7, $8)
on conflict (id)
do update set
    first_name = excluded.first_name,
    last_name = excluded.last_name,
    active = excluded.active,
    position = excluded.position,
    team = excluded.team,
    status = excluded.status,
    injury_status = excluded.injury_status
"#,
        )
        .bind(player.player_id)
        .bind(player.first_name)
        .bind(player.last_name)
        .bind(player.active)
        .bind(player.position)
        .bind(player.team)
        .bind(player.status)
        .bind(player.injury_status)
        .execute(&pg_pool)
        .await?;
    }
    Ok(())
}

#[derive(Debug, Deserialize)]
struct SleeperPlayer {
    player_id: String,
    first_name: String,
    last_name: String,
    active: bool,
    position: Option<String>,
    team: Option<String>,
    status: Option<String>,
    injury_status: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GetPlayersQuery {
    team: String,
}

#[derive(Debug, sqlx::FromRow, Serialize)]
struct GetPlayersResult {
    id: String,
    first_name: String,
    last_name: String,
    position: Option<String>,
    team: Option<String>,
}

async fn get_players(
    Query(query): Query<GetPlayersQuery>,
    Extension(pg_pool): Extension<PgPool>,
) -> Result<Json<Vec<GetPlayersResult>>, AppError> {
    let players = sqlx::query_as::<_, GetPlayersResult>(
        r#"
select id, first_name, last_name, position, team
from players
where active and team = $1
"#,
    )
    .bind(query.team)
    .fetch_all(&pg_pool)
    .await?;
    Ok(Json(players))
}
