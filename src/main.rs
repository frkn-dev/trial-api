use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use warp::Filter;

use std::{
    collections::HashMap,
    fs::{File, OpenOptions},
    io::{BufRead, BufReader, Write},
    sync::{Arc, Mutex},
};

use lettre::{
    transport::smtp::authentication::Credentials, AsyncSmtpTransport, AsyncTransport, Message,
    Tokio1Executor,
};
use reqwest::Client;

/* ================= CONFIG ================= */

const CSV_FILE: &str = "trials.csv";
const DEFAULT_DAYS: i64 = 1;
const DEFAULT_ENV: &str = "dev";

const PROTOS: [&str; 4] = [
    "VlessTcpReality",
    "VlessGrpcReality",
    "VlessXhttpReality",
    "Hysteria2",
];

/* ================= MODELS ================= */

#[derive(Debug, Deserialize)]
struct TrialRequest {
    email: String,
    telegram: Option<String>,
}

#[derive(Debug, Serialize)]
struct TrialResponse {
    status: String,
    message: String,
    sub_id: Option<String>,
}

use uuid::Uuid;

#[derive(Debug, Deserialize)]
pub struct ApiResponse<T> {
    pub status: u16,
    pub message: String,
    pub response: T,
}

/* ===== SUBSCRIPTION ===== */

#[derive(Debug, Deserialize)]
pub struct SubscriptionResponse {
    pub id: Uuid,
    pub instance: SubscriptionInstance,
}

#[derive(Debug, Deserialize)]
pub struct SubscriptionInstance {
    #[serde(rename = "Subscription")]
    pub subscription: Subscription,
}

#[derive(Debug, Deserialize)]
pub struct Subscription {
    pub id: Uuid,
    pub expires_at: String,
    pub referred_by: Option<Uuid>,
    pub created_at: String,
    pub updated_at: String,
    pub is_deleted: bool,
}

/* ===== CONNECTION ===== */

#[derive(Debug, Deserialize)]
pub struct ConnectionResponse {
    pub id: Uuid,
    pub instance: ConnectionInstance,
}

#[derive(Debug, Deserialize)]
pub struct ConnectionInstance {
    #[serde(rename = "Connection")]
    pub connection: Connection,
}

#[derive(Debug, Deserialize)]
pub struct Connection {
    pub env: String,
    pub proto: ProtoWrapper,
    pub stat: ConnectionStat,
    pub subscription_id: Uuid,
    pub created_at: String,
    pub modified_at: String,
    pub expired_at: Option<String>,
    pub is_deleted: bool,
    pub node_id: Option<Uuid>,
}

#[derive(Debug, Deserialize)]
pub struct ProtoWrapper {
    #[serde(rename = "Xray")]
    pub xray: String,
}

#[derive(Debug, Deserialize)]
pub struct ConnectionStat {
    pub downlink: u64,
    pub uplink: u64,
    pub online: u64,
}

type Store = Arc<Mutex<HashMap<String, DateTime<Utc>>>>;

/* ================= MAIN ================= */

#[tokio::main]
async fn main() {
    let store: Store = Arc::new(Mutex::new(load_trials()));
    let store_filter = warp::any().map(move || store.clone());

    let cors = warp::cors()
        .allow_any_origin()
        .allow_methods(vec!["POST", "OPTIONS"])
        .allow_headers(vec!["Content-Type"]);

    let route = warp::post()
        .and(warp::path("trial"))
        .and(warp::body::json())
        .and(store_filter)
        .and_then(handle_trial)
        .with(cors);

    println!("🚀 Trial service on 127.0.0.1:3030");
    warp::serve(route).run(([127, 0, 0, 1], 3030)).await;
}

/* ================= HANDLER ================= */

async fn handle_trial(
    req: TrialRequest,
    store: Store,
) -> Result<impl warp::Reply, warp::Rejection> {
    {
        let guard = store.lock().unwrap();
        if guard.contains_key(&req.email) {
            return Ok(warp::reply::json(&TrialResponse {
                status: "error".into(),
                message: "Trial already requested".into(),
                sub_id: None,
            }));
        }
    }

    let now = Utc::now();

    /* 1. CREATE SUBSCRIPTION */
    let sub_id = match create_subscription(DEFAULT_ENV, DEFAULT_DAYS).await {
        Ok(id) => id,
        Err(e) => {
            eprintln!("❌ subscription error: {}", e);
            return Ok(warp::reply::json(&TrialResponse {
                status: "error".into(),
                message: "Failed to create subscription".into(),
                sub_id: None,
            }));
        }
    };

    /* 2. CREATE CONNECTIONS */
    for proto in PROTOS {
        if proto == "Hysteria2" {
            let token = uuid::Uuid::new_v4();
            if let Err(e) = create_connection(DEFAULT_ENV, proto, &sub_id, &Some(token)).await {
                eprintln!("❌ connection {} error: {}", proto, e);
            }
        } else {
            if let Err(e) = create_connection(DEFAULT_ENV, proto, &sub_id, &None).await {
                eprintln!("❌ connection {} error: {}", proto, e);
            }
        }
    }

    /* 3. SAVE */
    {
        let mut guard = store.lock().unwrap();
        guard.insert(req.email.clone(), Utc::now());
    }

    save_trial(
        &req.email,
        req.telegram.as_deref(),
        &sub_id,
        DEFAULT_ENV,
        &now,
    )
    .ok();

    /* 4. EMAIL */
    if let Err(e) = send_email(&req.email, &sub_id).await {
        eprintln!("📧 email error: {}", e);
    }

    Ok(warp::reply::json(&TrialResponse {
        status: "ok".into(),
        message: "Trial activated. Check your email.".into(),
        sub_id: Some(sub_id.to_string()),
    }))
}

/* ================= FRKN API ================= */

async fn client() -> Client {
    Client::new()
}

fn auth_headers(req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
    req.header(
        "Authorization",
        format!("Bearer {}", std::env::var("FRKN_API_TOKEN").unwrap()),
    )
    .header("Accept", "application/json")
    .header("Content-Type", "application/json")
}

pub async fn create_subscription(env: &str, days: i64) -> anyhow::Result<Uuid> {
    let host = std::env::var("FRKN_HOST")?;
    let cli = Client::new();

    let res = auth_headers(
        cli.post(format!("{}/subscription", host))
            .json(&serde_json::json!({
                "env": env,
                "days": days
            })),
    )
    .send()
    .await?;

    let status = res.status();
    let text = res.text().await?;

    if !status.is_success() {
        anyhow::bail!("API error {}: {}", status, text);
    }

    let parsed: ApiResponse<SubscriptionResponse> = serde_json::from_str(&text)?;

    Ok(parsed.response.id)
}

pub async fn create_connection(
    env: &str,
    proto: &str,
    sub_id: &Uuid,
    token: &Option<Uuid>,
) -> anyhow::Result<String> {
    let host = std::env::var("FRKN_HOST")?;
    let cli = client();

    let res = if let Some(token) = token {
        auth_headers(
            cli.await
                .post(format!("{}/connection", host))
                .json(&serde_json::json!({
                    "env": env,
                    "proto": proto,
                    "subscription_id": sub_id,
                    "token": token,
                })),
        )
        .send()
        .await?
    } else {
        auth_headers(
            cli.await
                .post(format!("{}/connection", host))
                .json(&serde_json::json!({
                    "env": env,
                    "proto": proto,
                    "subscription_id": sub_id
                })),
        )
        .send()
        .await?
    };

    let status = res.status();
    let text = res.text().await?;

    if text.is_empty() {
        anyhow::bail!("empty connection response, status = {}", status);
    }

    let parsed: ApiResponse<ConnectionResponse> = serde_json::from_str(&text)?;

    Ok(parsed.response.id.to_string())
}

/* ================= EMAIL ================= */

async fn send_email(to: &str, sub_id: &Uuid) -> Result<(), Box<dyn std::error::Error>> {
    let user = std::env::var("GMAIL_USER")?;
    let pass = std::env::var("GMAIL_APP_PASSWORD")?;
    let host = std::env::var("FRKN_HOST")?;

    // HTML письмо
    let html_body = format!(
        r#"
<!DOCTYPE html>
<html>
<head>
<meta charset="UTF-8">
<title>FRKN VPN Trial</title>
<style>
    body {{
        font-family: Arial, sans-serif;
        background-color: #f4f4f4;
        margin: 0;
        padding: 0;
    }}
    .container {{
        width: 100%;
        max-width: 600px;
        margin: 0 auto;
        background-color: #ffffff;
        padding: 20px;
        border-radius: 12px;
        box-shadow: 0 4px 12px rgba(0,0,0,0.1);
    }}
    .header {{
        text-align: center;
        margin-bottom: 20px;
    }}
    .logo {{
        max-width: 150px;
    }}
    h1 {{
        color: #1d4ed8; /* фирменный синий */
        font-size: 24px;
    }}
    p {{
        color: #374151;
        font-size: 16px;
        line-height: 1.5;
    }}
    .button {{
        display: inline-block;
        padding: 12px 24px;
        background-color: #1d4ed8;
        color: #ffffff;
        text-decoration: none;
        border-radius: 8px;
        margin: 20px 0;
        font-weight: bold;
    }}
    .footer {{
        font-size: 12px;
        color: #9ca3af;
        text-align: center;
        margin-top: 20px;
    }}
</style>
</head>
<body>
<div class="container">
    <div class="header">
        
        <h1>Твой триал активирован!</h1>
    </div>
    <p>Привет!</p>
    <p>Твой триал для <strong>FRKN</strong> успешно активирован 🎉</p>
    <p>Информация по подписке:</p>
    <p>
        <strong>ID:</strong> {sub_id}<br/>
        <strong>Ссылка:</strong> <a href="{host}/sub/info?id={sub_id}">{host}/sub/info?id={sub_id}</a>
    </p>
    <a href="{host}/sub/info?id={sub_id}"

   style="
       display: inline-block;
       padding: 12px 24px;
       background-color: #1d4ed8;
       color: #ffffff !important;
       text-decoration: none;
       border-radius: 8px;
       font-weight: bold;
   ">
   Перейти к подписке
</a>



    <p>Подписывайся на наш Telegram: <a href="https://t.me/frkn_org">@frkn_org</a></p>
    <div class="footer"> <a href="https://t.me/frkn_support">Поддержка</a></p> <br>
        Vive la résistance!<br/>
        © 2026 FRKN
    </div>
</div>
</body>
</html>
"#,
        host = host,
        sub_id = sub_id
    );

    let msg = Message::builder()
        .from(format!("FRKN <{}>", user).parse()?)
        .to(to.parse()?)
        .subject("FRKN VPN Trial 🚀")
        .header(lettre::message::header::ContentType::TEXT_HTML)
        .body(html_body)?;

    let mailer = AsyncSmtpTransport::<Tokio1Executor>::relay("smtp.gmail.com")?
        .credentials(Credentials::new(user.clone(), pass))
        .build();

    mailer.send(msg).await?;
    Ok(())
}

/* ================= CSV ================= */

fn save_trial(
    email: &str,
    tg: Option<&str>,
    sub_id: &Uuid,
    env: &str,
    time: &DateTime<Utc>,
) -> std::io::Result<()> {
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(CSV_FILE)?;

    writeln!(
        file,
        "{},{},{},{},{}",
        time.to_rfc3339(),
        email,
        tg.unwrap_or(""),
        sub_id,
        env
    )?;

    Ok(())
}

fn load_trials() -> HashMap<String, DateTime<Utc>> {
    let mut map = HashMap::new();

    if let Ok(file) = File::open(CSV_FILE) {
        for line in BufReader::new(file).lines().flatten() {
            let parts: Vec<_> = line.split(',').collect();
            if parts.len() >= 2 {
                if let Ok(ts) = parts[0].parse::<DateTime<Utc>>() {
                    map.insert(parts[1].to_string(), ts);
                }
            }
        }
    }
    map
}
