use axum::{
    body::Bytes,
    extract::{Query, State},
    http::{HeaderMap, Response, StatusCode},
    response::IntoResponse,
    routing::{get, post},
};
use serde::Deserialize;
use serde_json::json;
use std::{
    future::IntoFuture,
    io::{BufWriter, Write},
    sync::Arc,
};
use wasmtime::{Caller, Engine, Func, Instance, Module, Store};

#[derive(Deserialize, Debug, Clone)]
struct AppConfig {
    port: u16,
    hass_access_token: String,
    hass_endpoint: String,
    hass_entity_id: String,
    geoip_db: String,
    ddp_endpoint: String,
    fps: usize,
}

#[derive(Clone)]
struct AppState {
    geoip_db: Arc<maxminddb::Reader<Vec<u8>>>,
    simple_tx: flume::Sender<SimpleLightParams>,
    wasm_tx: tokio::sync::mpsc::Sender<Vec<u8>>,
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

const SIZE: usize = 600;
const DDP_CHUNK_SIZE: usize = 480;
type Canvas = [u8; SIZE * 3];

async fn send_ddp(
    udp: Arc<tokio::net::UdpSocket>,
    data: &[u8],
    offset: usize,
    seq: u8,
) -> anyhow::Result<()> {
    // first two bytes are version
    let byte_0 = 0b0100_0001u8;

    // last four bits of byte_0 are the sequence number
    let byte_1 = seq;

    // two other bits, three bits for type (RGB), and three bits for bpp
    let byte_2 = 0b00000001u8;

    // destination ID
    let byte_3 = 255;

    // offset: 32-bit number, MSB first
    // len: 16-bit number, MSB first
    // then data

    let mut packet = BufWriter::new(Vec::with_capacity(10 + data.len()));
    packet.write_all(&[byte_0, byte_1, byte_2, byte_3])?;
    packet.write_all(&(offset as u32).to_be_bytes())?;
    packet.write_all(&(data.len() as u16).to_be_bytes())?;
    packet.write_all(data)?;

    let data = packet.into_inner()?;
    udp.send(&data).await?;

    Ok(())
}

async fn run_program(
    mut kill: tokio::sync::mpsc::Receiver<()>,
    wasm: Vec<u8>,
    conn: Arc<tokio::net::UdpSocket>,
    fps: usize,
) -> anyhow::Result<()> {
    let mut config = wasmtime::Config::new();
    config.consume_fuel(true);
    let engine = Engine::new(&config)?;

    let canvas = [0u8; SIZE * 3];
    let module = Module::from_binary(&engine, &wasm)?;
    let mut store = Store::new(&engine, canvas);

    let set_pixel = Func::wrap(
        &mut store,
        |mut caller: Caller<'_, Canvas>, idx: u32, color: u32| {
            if idx < SIZE as u32 {
                for i in 0..3 {
                    let idx = (idx as usize * 3) + i;
                    caller.data_mut()[idx] = ((color >> (8 * (2 - i))) & 0xFF) as u8;
                }
            }
        },
    );

    let get_pixel = Func::wrap(
        &mut store,
        |mut caller: Caller<'_, Canvas>, idx: u32| -> u32 {
            if idx < SIZE as u32 {
                let mut color = 0;
                for i in 0..3 {
                    let idx = (idx as usize * 3) + i;
                    color |= (caller.data_mut()[idx] as u32) << (8 * (2 - i));
                }
                color
            } else {
                0
            }
        },
    );

    let size = Func::wrap(&mut store, |_: Caller<'_, Canvas>| -> u32 { SIZE as u32 });
    let time = Func::wrap(&mut store, |_: Caller<'_, Canvas>| -> u64 {
        std::time::Instant::now().elapsed().as_millis() as u64
    });

    let imports = module.imports().collect::<Vec<_>>();
    let mut added_imports = vec![];
    for import in imports {
        match import.name() {
            "set_pixel" => added_imports.push(set_pixel.into()),
            "get_pixel" => added_imports.push(get_pixel.into()),
            "size" => added_imports.push(size.into()),
            "time" => added_imports.push(time.into()),
            _ => {}
        }
    }

    let instance = Instance::new(&mut store, &module, &added_imports)?;

    let tick = instance.get_typed_func::<f32, ()>(&mut store, "tick")?;

    let start_time = std::time::Instant::now();
    let frame = std::time::Duration::from_millis(1000 / fps as u64);

    let mut seq = 0;
    while kill.try_recv().is_err() {
        store.set_fuel(10_000_000)?;

        let now = std::time::Instant::now();
        let elapsed = (now - start_time).as_secs_f32();
        tick.call(&mut store, elapsed)?;

        for (i, chunk) in store.data().chunks(DDP_CHUNK_SIZE * 3).enumerate() {
            send_ddp(conn.clone(), chunk, i * DDP_CHUNK_SIZE * 3, seq).await?;
            seq += 1;
            if seq > 15 {
                seq = 0;
            }
        }

        let sleep_req = frame.checked_sub(now.elapsed());
        if let Some(sleep) = sleep_req {
            tokio::time::sleep(sleep).await;
        }
    }

    Ok(())
}

async fn process(
    ddp: String,
    mut rx: tokio::sync::mpsc::Receiver<Vec<u8>>,
    fps: usize,
) -> anyhow::Result<()> {
    let mut last_kill_tx: Option<tokio::sync::mpsc::Sender<()>> = None;
    let conn = tokio::net::UdpSocket::bind("0.0.0.0:4049").await?;
    let conn = Arc::new(conn);
    conn.connect(ddp).await?;

    loop {
        let timeout = tokio::time::sleep(tokio::time::Duration::from_secs(30));
        tokio::select! {
            _ = timeout => {
                if last_kill_tx.is_some() {
                    last_kill_tx.take().unwrap().send(()).await.ok();
                }
            },

            Some(wasm) = rx.recv() => {
                if last_kill_tx.is_some() {
                    last_kill_tx.take().unwrap().send(()).await.ok();
                }

                let (kill_tx, kill_rx) = tokio::sync::mpsc::channel::<()>(1);
                last_kill_tx = Some(kill_tx.clone());

                let conn = conn.clone();
                tokio::spawn(async move {
                    println!("running new program");
                    if let Err(e) = run_program(kill_rx, wasm, conn, fps).await {
                        eprintln!("{:?}", e);
                    }
                });
            }
        }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let config_path = std::env::args().nth(1).unwrap_or("config.json".to_string());
    let config = std::fs::read_to_string(config_path)?;
    let config: AppConfig = serde_json::from_str(&config)?;

    let (simple_tx, simple_rx) = flume::unbounded::<SimpleLightParams>();
    let config_recv = config.clone();
    let recv_task = tokio::task::spawn(async move {
        let client = reqwest::Client::new();

        loop {
            // Clean the backlog
            let mut msg = None;
            while let Ok(new_msg) = simple_rx.try_recv() {
                msg = Some(new_msg);
            }

            if let Some(SimpleLightParams(r, g, b, a)) = msg {
                if a == 0 {
                    let body = json!({
                        "entity_id": config_recv.hass_entity_id,
                    });

                    client
                        .post(format!(
                            "{}/services/light/turn_off",
                            config_recv.hass_endpoint
                        ))
                        .bearer_auth(&config_recv.hass_access_token)
                        .json(&body)
                        .send()
                        .await
                        .ok();
                } else {
                    let body = json!({
                        "entity_id": config_recv.hass_entity_id,
                        "rgb_color": [r, g, b],
                        "brightness": a
                    });

                    client
                        .post(format!(
                            "{}/services/light/turn_on",
                            config_recv.hass_endpoint
                        ))
                        .bearer_auth(&config_recv.hass_access_token)
                        .json(&body)
                        .send()
                        .await
                        .ok();
                }
            } else {
                tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
                continue;
            }
        }
    });

    let (wasm_tx, wasm_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(1);
    let ddp = config.ddp_endpoint.clone();
    let fps = config.fps;
    tokio::spawn(async move {
        process(ddp, wasm_rx, fps).await.unwrap();
    });

    let db = std::fs::read(config.geoip_db.clone())?;
    let reader = maxminddb::Reader::from_source(db)?;

    let app_state = AppState {
        geoip_db: Arc::new(reader),
        simple_tx,
        wasm_tx,
    };

    let app = axum::Router::new()
        .route("/", get(simple))
        .route("/", post(wasm))
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

fn log_request(headers: HeaderMap, state: State<AppState>, query: &str) {
    let ip = headers
        .get("X-Forwarded-For")
        .and_then(|x| x.to_str().ok())
        .unwrap_or("unknown");
    let reader = state.geoip_db.as_ref();
    let ip_region = get_region(ip, reader).ok().unwrap_or("unknown".to_string());
    let ua = headers
        .get("User-Agent")
        .and_then(|x| x.to_str().ok())
        .unwrap_or("unknown");

    println!("{} [{}] ({}) - {:?}", ip, ip_region, ua, query);
}

async fn simple(
    State(state): State<AppState>,
    Query(query): Query<LightParams>,
    headers: HeaderMap,
) -> AppResponse {
    let simple = SimpleLightParams(query.r, query.g, query.b, query.a.unwrap_or(query.bri));
    log_request(
        headers,
        State(state.clone()),
        format!("{:?}", simple).as_str(),
    );
    state.simple_tx.send(simple)?;
    Ok(Response::builder()
        .status(StatusCode::OK)
        .body("Lights updated, thanks for torturing me".into())
        .unwrap())
}

async fn wasm(State(state): State<AppState>, headers: HeaderMap, wasm: Bytes) -> AppResponse {
    log_request(headers, State(state.clone()), "wasm");
    state.wasm_tx.send(wasm.to_vec()).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

#[derive(Deserialize, Debug)]
struct LightParams {
    r: u8,
    g: u8,
    b: u8,
    a: Option<u8>,
    #[serde(default = "default_brightness")]
    bri: u8,
}

#[derive(Debug)]
struct SimpleLightParams(u8, u8, u8, u8);

fn default_brightness() -> u8 {
    255
}
