use axum::{
    extract::{Query, State},
    http::{HeaderMap, Response, StatusCode},
    response::IntoResponse,
    routing::get,
};
use serde::Deserialize;
use serde_json::json;
use std::{future::IntoFuture, sync::Arc};

#[derive(Deserialize, Debug, Clone)]
struct Config {
    port: u16,
    hass_access_token: String,
    hass_endpoint: String,
    hass_entity_id: String,
    geoip_db: Option<String>,
}

#[derive(Clone)]
struct AppState {
    geoip_db: Option<Arc<maxminddb::Reader<Vec<u8>>>>,
    msg_tx: flume::Sender<LightParams>,
}

struct AppError(anyhow::Error);
type AppResult<T> = axum::response::Result<T, AppError>;
type AppResponse = AppResult<Response<axum::body::Body>>;
impl IntoResponse for AppError {
    fn into_response(self) -> axum::response::Response {
        (StatusCode::INTERNAL_SERVER_ERROR, self.0.to_string()).into_response()
    }
}
impl<E> From<E> for AppError
where
    E: Into<anyhow::Error>,
{
    fn from(err: E) -> Self {
        Self(err.into())
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let config_path = std::env::args().nth(1).unwrap_or("config.json".to_string());
    let config = std::fs::read_to_string(config_path)?;
    let config: Config = serde_json::from_str(&config)?;

    let (tx, rx) = flume::unbounded::<LightParams>();
    let config_recv = config.clone();
    let recv_task = tokio::task::spawn(async move {
        let client = reqwest::Client::new();

        loop {
            // Clean the backlog
            let mut msg = None;
            while let Ok(new_msg) = rx.try_recv() {
                msg = Some(new_msg);
            }

            if let Some(msg) = msg {
                let body = json!({
                    "entity_id": config_recv.hass_entity_id,
                    "rgb_color": [msg.r, msg.g, msg.b],
                    "brightness": msg.bri
                });

                client
                    .post(&config_recv.hass_endpoint)
                    .bearer_auth(&config_recv.hass_access_token)
                    .json(&body)
                    .send()
                    .await
                    .ok();
            } else {
                tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
                continue;
            }
        }
    });

    let app_state = AppState {
        geoip_db: config.geoip_db.map(|path| {
            let db = std::fs::read(path).unwrap();
            let reader = maxminddb::Reader::from_source(db).unwrap();
            Arc::new(reader)
        }),
        msg_tx: tx,
    };

    let app = axum::Router::new()
        .route("/", get(handler))
        .with_state(app_state);
    let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{}", config.port))
        .await
        .unwrap();
    let server = axum::serve(listener, app).into_future();
    tokio::select! {
        _ = server => {},
        _ = recv_task => {},
    }

    Ok(())
}

fn get_region(ip: &str, reader: &maxminddb::Reader<Vec<u8>>) -> anyhow::Result<String> {
    let ip: std::net::IpAddr = ip.parse()?;
    let city: maxminddb::geoip2::City = reader.lookup(ip)?;
    let region = city
        .subdivisions
        .and_then(|x| x.first().cloned())
        .map(|x| x.names.as_ref().unwrap().get("en").unwrap().to_string())
        .unwrap_or("unknown".to_string())
        .to_string();
    let country = city
        .country
        .and_then(|x| x.names.as_ref().unwrap().get("en").copied())
        .unwrap_or("unknown");
    Ok(format!("{}, {}", region, country))
}

async fn handler(
    State(state): State<AppState>,
    Query(query): Query<LightParams>,
    headers: HeaderMap,
) -> AppResponse {
    let ip = headers
        .get("X-Forwarded-For")
        .map(|x| x.to_str().unwrap())
        .unwrap_or("unknown");

    let ip_region = state
        .geoip_db
        .as_ref()
        .and_then(|reader| get_region(ip, reader).ok())
        .unwrap_or("unknown".to_string());

    let ua = headers
        .get("User-Agent")
        .map(|x| x.to_str().unwrap())
        .unwrap_or("unknown");

    println!("{} [{}] ({}) - {:?}", ip, ip_region, ua, query);
    state.msg_tx.send(query).unwrap();
    Ok(StatusCode::NO_CONTENT.into_response())
}

#[derive(Deserialize, Debug)]
struct LightParams {
    r: u8,
    g: u8,
    b: u8,
    #[serde(default = "default_brightness")]
    bri: u8,
}

fn default_brightness() -> u8 {
    255
}
